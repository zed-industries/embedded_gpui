//! The object registry: the entire object model, compiled identically into both ends of
//! the boundary.
//!
//! A registry knows exactly two things: *local* objects (homes -- entities whose state
//! lives here, with this end's root at address 0) and *remote* objects (projections of
//! the other end's homes). There is no notion of host or guest anywhere in this module:
//! the two ends differ only in the transport they construct the registry with -- a sink
//! that carries outgoing [`Frame`]s. Boundary creation supplies it; the object model
//! never mentions it again.
//!
//! Ids are random u64s, globally unique for practical purposes, so a ref is universally
//! applicable: nothing is namespaced per end, and an id can only be *known*, never
//! guessed or enumerated — holding a ref is the authority. Refs travel in each frame's
//! ref table, so payloads stay opaque here while every capability in transit is
//! enumerable to whoever forwards it. The single reserved value is 0, "your root": a
//! connection-local address (never an identity in a payload) that each end answers with
//! its own root object.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gpui::{AnyEntity, App, AppContext as _, Entity, Subscription};

use crate::{
    HandlerResponse, Interface, MethodHandler, Methods, NOTIFY_EVENT, Payload, RawEvent, Ref,
    Remote, RemoteSignal, ResponseSender, Shared,
};

/// The reserved connection-local address meaning "the root object of whichever end you
/// send it to". Never minted as an object id and never meaningful inside a payload.
const ROOT_ADDRESS: u64 = 0;

/// A method call toward a home on the other end.
pub(crate) struct Call {
    pub target: u64,
    pub request: Option<u64>,
    pub method: String,
    pub payload: Payload,
    /// For a call whose response is a ref: the id the caller minted for the object that
    /// ref will name, so it can address the object before the response arrives (promise
    /// pipelining). The home installs the returned object under this id too.
    pub promised: Option<u64>,
}

/// The answer to a [`Call`] that carried a `request` id.
pub(crate) struct Response {
    pub request: u64,
    pub outcome: Result<Payload, String>,
}

/// Everything that crosses the boundary about objects, in wire-neutral form (the
/// boundary layer converts to its bindgen records). Calls and responses carry payloads
/// with ref tables; subscribe and release are the two operations *constitutive* of
/// objecthood — structural frames, not reserved method names, so the method namespace
/// belongs entirely to user schemas. Both directions are one FIFO of these.
pub(crate) enum Frame {
    Call(Call),
    Response(Response),
    Subscribe { target: u64, observer: u64 },
    Release { target: u64 },
}

/// How outgoing frames leave this end: the single point of side-specific behavior,
/// supplied at boundary creation.
pub(crate) type WireSink = Box<dyn Fn(Frame)>;

/// A local object: an entity homed on this end, with its dispatch closure. The
/// registry stores exactly one handler per object — how it interprets method names is
/// the object's business (usually a [`Methods`] table, by convention).
struct HomeEntry {
    /// Interface name (`std::any::type_name`), kept for diagnostics and as node labels
    /// for the (future) object-graph inspector.
    #[allow(dead_code)]
    type_name: &'static str,
    handler: MethodHandler,
    /// Observer objects on the other end (registered via `subscribe`); the home's
    /// `cx.notify` and typed events are sent to each as ordinary calls. Cleared on
    /// `release`.
    observers: Vec<u64>,
    /// The entity itself, so scenes and other side traffic addressed by object id can
    /// find it. Strong while the other end holds a remote (unless the type opted out of
    /// [`Shared::keep_alive`]); observer objects have none.
    entity: HomeEntity,
    /// The notify observation plus any typed-event forwarders wired at share time.
    _subscriptions: Vec<Subscription>,
    /// Wires the same notify and event forwarders under another id, so the entry can be
    /// installed again for a promised id. `None` for observer objects.
    resubscribe: Option<Resubscribe>,
}

type Resubscribe = Rc<dyn Fn(&Objects, u64, &mut App) -> Vec<Subscription>>;
type EventsFactory = Rc<dyn Fn(crate::EventSink, &mut App) -> Vec<Subscription>>;

#[derive(Clone)]
enum HomeEntity {
    None,
    Strong(AnyEntity),
    Weak(gpui::AnyWeakEntity),
}

impl HomeEntity {
    // Only the host routes by object id today (scenes to surfaces).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn upgrade(&self) -> Option<AnyEntity> {
        match self {
            Self::None => None,
            Self::Strong(entity) => Some(entity.clone()),
            Self::Weak(entity) => entity.upgrade(),
        }
    }

    /// The other end released the object: stop keeping it alive.
    fn release(&mut self) {
        if let Self::Strong(entity) = self {
            *self = Self::Weak(entity.downgrade());
        }
    }
}

/// A remote object: this end's projection of an entity homed on the other end.
struct Projection {
    /// Interface name as first connected, for a diagnostic on mismatched reconnects.
    type_name: &'static str,
    /// Where the home's notifies and events land locally, plus the id of the hidden
    /// observer object that receives them. Created lazily by the first
    /// `observe`/`subscribe`, which is also when `subscribe` is sent — a projection
    /// nobody listens to costs the wire nothing.
    signal: Option<(Entity<RemoteSignal>, u64)>,
    /// Live while some `Remote` still holds the projection; used to hand the same guard
    /// back when the same ref is connected twice.
    guard: Weak<ReleaseGuard>,
}

/// Dropping the last `Remote` for a projection queues a release: the home end drops its
/// strong handle and events stop flowing. Queued (not sent inline) because drops can
/// happen anywhere, including mid-dispatch; the queue is drained from the boundary's
/// pump.
struct ReleaseGuard {
    entity_id: u64,
    queue: Rc<RefCell<Vec<u64>>>,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.queue.borrow_mut().push(self.entity_id);
    }
}

#[derive(Default)]
struct State {
    homes: HashMap<u64, HomeEntry>,
    projections: HashMap<u64, Projection>,
    next_request_id: u64,
    pending_responses: HashMap<u64, ResponseSender>,
    /// Promised ids whose allocating call has not answered yet, with the frames that
    /// arrived for them meanwhile, delivered in order once the object exists.
    pending_promises: HashMap<u64, Vec<Frame>>,
    /// Promised ids whose allocating call failed: every call through them fails with
    /// that reason until the caller releases the id.
    failed_promises: HashMap<u64, String>,
    /// Frames addressed to this end's root before `share_root` ran. The other end's
    /// bootstrap may lawfully race ours, so root traffic queues instead of failing;
    /// `share_root` drains it in order.
    pending_root_frames: Vec<Frame>,
}

struct Inner {
    sink: WireSink,
    /// Projections whose last `Remote` dropped; drained into `release` frames.
    releases: Rc<RefCell<Vec<u64>>>,
    state: RefCell<State>,
    /// Side channels the boundary installs beside the object model (the host's
    /// synchronous input queries), found by type. The registry never reads them.
    extensions: RefCell<HashMap<std::any::TypeId, Rc<dyn std::any::Any>>>,
}

/// A handle to one end's object registry. Clones share the registry; the boundary layer
/// owns one and the transports/guards of live remotes hold the others.
#[derive(Clone)]
pub(crate) struct Objects {
    inner: Rc<Inner>,
}

/// A non-owning handle for everything the registry itself stores (subscriptions, event
/// sinks) or that user entities may hold indefinitely (every `Remote` and `Ref`): a
/// strong capture there would cycle through `Inner` and keep every shared object alive
/// forever. Remotes outliving their boundary resolve receipts with errors.
#[derive(Clone)]
pub(crate) struct WeakObjects {
    inner: Weak<Inner>,
}

impl WeakObjects {
    pub(crate) fn upgrade(&self) -> Option<Objects> {
        Some(Objects {
            inner: self.inner.upgrade()?,
        })
    }

    /// A handle to no registry at all.
    pub(crate) fn detached() -> Self {
        Self { inner: Weak::new() }
    }
}

impl Objects {
    pub(crate) fn downgrade(&self) -> WeakObjects {
        WeakObjects {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub fn new(sink: WireSink) -> Self {
        Self {
            inner: Rc::new(Inner {
                sink,
                releases: Rc::default(),
                state: RefCell::new(State::default()),
                extensions: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// Install a side channel reachable from every ref and remote of this registry.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn set_extension<T: 'static>(&self, value: Rc<T>) {
        self.inner
            .extensions
            .borrow_mut()
            .insert(std::any::TypeId::of::<T>(), value);
    }

    pub fn extension<T: 'static>(&self) -> Option<Rc<T>> {
        let extensions = self.inner.extensions.borrow();
        let value = extensions.get(&std::any::TypeId::of::<T>())?.clone();
        value.downcast::<T>().ok()
    }

    /// Install `entity` as this end's root object (address 0). The other end reaches it
    /// with `root()`; everything else it can reach is a method of this object
    /// returning refs. Root traffic that arrived first was queued and is delivered
    /// now, in order, so the two bootstraps may race freely.
    pub fn share_root<S, T>(&self, entity: &Entity<T>, cx: &mut App)
    where
        S: Interface,
        T: Shared<S>,
    {
        let entity_id = ROOT_ADDRESS;
        if self.inner.state.borrow().homes.contains_key(&entity_id) {
            log::warn!("embedded_gpui: root object replaced");
        }
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let events = T::events(entity, self.event_sink(entity_id), cx);
        let events_factory: EventsFactory = {
            // Weak: a home entry must not keep its entity alive past release.
            let entity = entity.downgrade();
            Rc::new(move |sink, cx| match entity.upgrade() {
                Some(entity) => T::events(&entity, sink, cx),
                None => Vec::new(),
            })
        };
        self.install::<S, T>(
            entity,
            methods,
            events,
            Some(events_factory),
            entity_id,
            T::keep_alive(),
            cx,
        );
        let queued = std::mem::take(&mut self.inner.state.borrow_mut().pending_root_frames);
        for frame in queued {
            self.deliver(frame, cx);
        }
    }

    /// Share a local entity, returning a capability reference to embed in message or
    /// event payloads. The registry holds the entity alive until the other end's last
    /// remote drops, unless `T` opts out via [`Shared::keep_alive`].
    pub fn share<S, T>(&self, entity: &Entity<T>, cx: &mut App) -> Ref<S>
    where
        S: Interface,
        T: Shared<S>,
    {
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let entity_id = self.reserve_local_id();
        let events = T::events(entity, self.event_sink(entity_id), cx);
        let events_factory: EventsFactory = {
            // Weak: a home entry must not keep its entity alive past release.
            let entity = entity.downgrade();
            Rc::new(move |sink, cx| match entity.upgrade() {
                Some(entity) => T::events(&entity, sink, cx),
                None => Vec::new(),
            })
        };
        self.install::<S, T>(
            entity,
            methods,
            events,
            Some(events_factory),
            entity_id,
            T::keep_alive(),
            cx,
        );
        Ref::new(entity_id, self.downgrade())
    }

    /// [`Objects::share`] with a closure-registered dispatch table instead of a schema
    /// interface: the dynamic escape hatch. `cx.notify` still crosses; typed events are
    /// not wired (implement [`Shared`] manually if you need both).
    pub fn share_with<S, T>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut App,
    ) -> Ref<S>
    where
        S: Interface,
        T: 'static,
    {
        let mut methods = Methods::new(entity.downgrade());
        register(&mut methods);
        let entity_id = self.reserve_local_id();
        self.install::<S, T>(entity, methods, Vec::new(), None, entity_id, true, cx);
        Ref::new(entity_id, self.downgrade())
    }

    /// Attach to the other end's root object (the reserved address 0 means "your root"
    /// from either direction). Returns immediately; the other end queues root traffic
    /// until its root is installed.
    pub fn root<S: Interface>(&self) -> Remote<S> {
        self.connect_id(ROOT_ADDRESS)
    }

    /// Whether `id` is an object homed on this end.
    pub fn is_local(&self, id: u64) -> bool {
        self.inner.state.borrow().homes.contains_key(&id)
    }

    /// The entity behind a local object id, if it is a `T` and still alive: how side
    /// traffic addressed by object id (a scene for a surface) finds its target.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn local_entity<T: 'static>(&self, id: u64) -> Option<Entity<T>> {
        let entity = self.inner.state.borrow().homes.get(&id)?.entity.upgrade()?;
        entity.downcast::<T>().ok()
    }

    /// Every live local entity of type `T`: how the boundary reaches all the surfaces
    /// it shared when the plugin behind them stops.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn local_entities<T: 'static>(&self) -> Vec<Entity<T>> {
        self.inner
            .state
            .borrow()
            .homes
            .values()
            .filter_map(|home| home.entity.upgrade()?.downcast::<T>().ok())
            .collect()
    }

    /// Attach to an entity by id. Connecting the same id twice returns a handle to the
    /// same projection; when the last clone drops, the home end is told to release the
    /// entity. Context-free: connecting allocates nothing but a map entry.
    pub(crate) fn connect_id<S: Interface>(&self, entity_id: u64) -> Remote<S> {
        if entity_id != ROOT_ADDRESS && self.is_local(entity_id) {
            log::warn!(
                "embedded_gpui: connecting a ref homed on this end (loopback) is not supported"
            );
        }
        let existing = {
            let state = self.inner.state.borrow();
            state
                .projections
                .get(&entity_id)
                .and_then(|projection| Some((projection.guard.upgrade()?, projection.type_name)))
        };
        if let Some((guard, type_name)) = existing {
            let requested = crate::interface_name::<S>();
            let opaque = crate::interface_name::<crate::Opaque>();
            if type_name != requested && type_name != opaque && requested != opaque {
                log::error!(
                    "embedded_gpui: object {entity_id} connected as {requested} but already \
                     live as {type_name}"
                );
            }
            return Remote::from_parts(self.downgrade(), entity_id, Some(guard));
        }
        // A projection whose guard died but whose release hasn't drained yet is stale;
        // releasing now and resubscribing below keeps the home end consistent.
        if self
            .inner
            .state
            .borrow()
            .projections
            .contains_key(&entity_id)
        {
            self.inner
                .releases
                .borrow_mut()
                .retain(|pending| *pending != entity_id);
            self.release_id(entity_id);
        }
        let guard = Rc::new(ReleaseGuard {
            entity_id,
            queue: self.inner.releases.clone(),
        });
        self.inner.state.borrow_mut().projections.insert(
            entity_id,
            Projection {
                type_name: crate::interface_name::<S>(),
                signal: None,
                guard: Rc::downgrade(&guard),
            },
        );
        Remote::from_parts(self.downgrade(), entity_id, Some(guard))
    }

    /// The signal a projection's notifies and events land on, created on first demand
    /// (from `observe`/`subscribe`, which have a context). Creation mints the hidden
    /// observer object that receives them as ordinary calls, and sends
    /// `subscribe(observer)` — subscription is lazy, so a projection nobody listens to
    /// costs the wire nothing. `None` if the projection is gone.
    pub(crate) fn signal_for(&self, entity_id: u64, cx: &mut App) -> Option<Entity<RemoteSignal>> {
        let existing = self
            .inner
            .state
            .borrow()
            .projections
            .get(&entity_id)?
            .signal
            .clone();
        if let Some((signal, _)) = existing {
            return Some(signal);
        }
        let signal = cx.new(|_| RemoteSignal::new());
        let observer_id = self.install_observer(signal.clone());
        self.inner
            .state
            .borrow_mut()
            .projections
            .get_mut(&entity_id)?
            .signal = Some((signal.clone(), observer_id));
        (self.inner.sink)(Frame::Subscribe {
            target: entity_id,
            observer: observer_id,
        });
        Some(signal)
    }

    /// Install the hidden observer object behind a projection's signal: a home whose
    /// handler turns incoming calls back into gpui reactivity — `$notify` becomes
    /// `cx.notify`, any other method name is a typed event for `cx.emit`.
    fn install_observer(&self, signal: Entity<RemoteSignal>) -> u64 {
        let entity_id = self.reserve_local_id();
        let handler: MethodHandler = Rc::new(move |method, payload, cx| {
            if method == NOTIFY_EVENT {
                signal.update(cx, |_, cx| cx.notify());
            } else {
                signal.update(cx, |_, cx| {
                    cx.emit(RawEvent {
                        name: method.to_string(),
                        payload: payload.clone(),
                    })
                });
            }
            HandlerResponse::Ready(Ok(Payload::empty()))
        });
        self.inner.state.borrow_mut().homes.insert(
            entity_id,
            HomeEntry {
                type_name: "observer",
                handler,
                observers: Vec::new(),
                entity: HomeEntity::None,
                _subscriptions: Vec::new(),
                resubscribe: None,
            },
        );
        entity_id
    }

    /// A call whose response is a ref, with the id for that ref minted here and now:
    /// the returned remote addresses the object before the response arrives, and calls
    /// through it queue FIFO behind the allocating call, so nothing waits a round trip
    /// (promise pipelining). `response` still resolves with the actual ref.
    pub(crate) fn promise_call<S: Interface>(
        &self,
        entity_id: u64,
        method: &str,
        payload: Payload,
        response: ResponseSender,
    ) -> Remote<S> {
        let promised = self.reserve_local_id();
        let remote = self.connect_id::<S>(promised);
        self.send(entity_id, method, payload, Some(response), Some(promised));
        remote
    }

    pub(crate) fn send(
        &self,
        entity_id: u64,
        method: &str,
        payload: Payload,
        response: Option<ResponseSender>,
        promised: Option<u64>,
    ) {
        let call = {
            let mut state = self.inner.state.borrow_mut();
            if !state.projections.contains_key(&entity_id) {
                // Dropping `response` here resolves the caller's receipt with an error.
                log::warn!("embedded_gpui: call to released object {entity_id}");
                return;
            }
            let request = response.map(|sender| {
                state.next_request_id += 1;
                let request = state.next_request_id;
                state.pending_responses.insert(request, sender);
                request
            });
            Call {
                target: entity_id,
                request,
                method: method.to_string(),
                payload,
                promised,
            }
        };
        (self.inner.sink)(Frame::Call(call));
    }

    /// Mint a fresh object id: random, nonzero, and unused here. Randomness is what
    /// makes refs universally applicable (no per-end namespaces, so they pass through
    /// any number of hands unrewritten) and unguessable (an id can only be learned
    /// from a payload that carried it; enumeration is infeasible). Collisions with ids
    /// minted by the other end are birthday-bounded at ~2^-64 per pair; the loopback
    /// check in `connect_id` doubles as the tripwire.
    fn reserve_local_id(&self) -> u64 {
        let state = self.inner.state.borrow();
        loop {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).expect("system entropy is unavailable");
            let id = u64::from_le_bytes(bytes);
            if id != ROOT_ADDRESS
                && !state.homes.contains_key(&id)
                && !state.projections.contains_key(&id)
            {
                return id;
            }
            log::error!("embedded_gpui: object id collision on mint; regenerating");
        }
    }

    /// The sink handed to schema-generated event wiring: it moves a home's typed events
    /// onto the wire. Weak because the registry stores the resulting subscriptions.
    fn event_sink(&self, entity_id: u64) -> crate::EventSink {
        let objects = self.downgrade();
        Rc::new(move |event: &str, payload: Payload, _cx: &mut App| {
            if let Some(objects) = objects.upgrade() {
                objects.notify_observers(entity_id, event, payload);
            }
        })
    }

    fn install<S, T>(
        &self,
        entity: &Entity<T>,
        methods: Methods<S, T>,
        event_forwarders: Vec<Subscription>,
        events_factory: Option<EventsFactory>,
        entity_id: u64,
        keep_alive: bool,
        cx: &mut App,
    ) where
        S: Interface,
        T: 'static,
    {
        let mut subscriptions = vec![self.observe_notify(entity, entity_id, cx)];
        subscriptions.extend(event_forwarders);
        let home_entity = if keep_alive {
            HomeEntity::Strong(entity.clone().into_any())
        } else {
            HomeEntity::Weak(entity.downgrade().into())
        };
        let resubscribe: Resubscribe = {
            // Weak: a home entry must not keep its entity alive past release.
            let entity = entity.downgrade();
            Rc::new(move |objects: &Objects, id: u64, cx: &mut App| {
                let Some(entity) = entity.upgrade() else {
                    return Vec::new();
                };
                let mut subscriptions = vec![objects.observe_notify(&entity, id, cx)];
                if let Some(factory) = &events_factory {
                    subscriptions.extend(factory(objects.event_sink(id), cx));
                }
                subscriptions
            })
        };
        self.inner.state.borrow_mut().homes.insert(
            entity_id,
            HomeEntry {
                type_name: crate::interface_name::<S>(),
                handler: methods.into_handler(),
                observers: Vec::new(),
                entity: home_entity,
                _subscriptions: subscriptions,
                resubscribe: Some(resubscribe),
            },
        );
    }

    /// The observation that turns the entity's `cx.notify` into notifies for the
    /// observers of `entity_id`.
    fn observe_notify<T: 'static>(
        &self,
        entity: &Entity<T>,
        entity_id: u64,
        cx: &mut App,
    ) -> Subscription {
        let objects = self.downgrade();
        cx.observe(entity, move |_, _| {
            if let Some(objects) = objects.upgrade() {
                objects.notify_observers(entity_id, NOTIFY_EVENT, Payload::empty());
            }
        })
    }

    /// Install the object homed at `from` under `to` as well: a second share of the same
    /// entity, with its own observers and lifetime. How a promised id becomes real.
    fn install_alias(&self, from: u64, to: u64, cx: &mut App) -> bool {
        let source = {
            let state = self.inner.state.borrow();
            let Some(home) = state.homes.get(&from) else {
                return false;
            };
            (
                home.type_name,
                home.handler.clone(),
                home.entity.clone(),
                home.resubscribe.clone(),
            )
        };
        let (type_name, handler, entity, resubscribe) = source;
        let subscriptions = resubscribe
            .as_ref()
            .map(|resubscribe| resubscribe(self, to, cx))
            .unwrap_or_default();
        self.inner.state.borrow_mut().homes.insert(
            to,
            HomeEntry {
                type_name,
                handler,
                observers: Vec::new(),
                entity,
                _subscriptions: subscriptions,
                resubscribe,
            },
        );
        true
    }

    /// The allocating call of promise `promised` answered: install the returned object
    /// under the promised id (or remember the failure), then deliver whatever arrived
    /// for it meanwhile, in order.
    fn resolve_promise(&self, promised: u64, outcome: &Result<Payload, String>, cx: &mut App) {
        let result = match outcome {
            Ok(payload) => match payload.refs.first() {
                Some(actual) if self.install_alias(*actual, promised, cx) => Ok(()),
                Some(actual) => Err(format!("promised object {actual} is not homed here")),
                None => Err("the call returned no ref to promise".to_string()),
            },
            Err(error) => Err(error.clone()),
        };
        let queued = {
            let mut state = self.inner.state.borrow_mut();
            let queued = state.pending_promises.remove(&promised).unwrap_or_default();
            if let Err(error) = &result {
                state.failed_promises.insert(promised, error.clone());
            }
            queued
        };
        for frame in queued {
            self.deliver(frame, cx);
        }
    }

    /// Where a frame addressed to a promised id stands: queued (the promise is pending),
    /// answered with the allocation's failure, or neither (a real object).
    fn intercept_promised(&self, frame: Frame) -> Option<Frame> {
        let target = match &frame {
            Frame::Call(call) => call.target,
            Frame::Subscribe { target, .. } | Frame::Release { target } => *target,
            Frame::Response(_) => return Some(frame),
        };
        let failure = {
            let mut state = self.inner.state.borrow_mut();
            if let Some(queue) = state.pending_promises.get_mut(&target) {
                queue.push(frame);
                return None;
            }
            match &frame {
                Frame::Release { .. } => state.failed_promises.remove(&target),
                _ => state.failed_promises.get(&target).cloned(),
            }
        };
        match failure {
            None => Some(frame),
            Some(reason) => {
                if let Frame::Call(call) = frame {
                    self.respond(call.request, Err(reason));
                }
                None
            }
        }
    }

    /// Send one notify or typed event from a local home to every observer object the
    /// other end registered: plain calls, fire-and-forget.
    fn notify_observers(&self, entity_id: u64, method: &str, payload: Payload) {
        let observers = self
            .inner
            .state
            .borrow()
            .homes
            .get(&entity_id)
            .map(|home| home.observers.clone())
            .unwrap_or_default();
        for observer in observers {
            (self.inner.sink)(Frame::Call(Call {
                target: observer,
                request: None,
                method: method.to_string(),
                payload: payload.clone(),
                promised: None,
            }));
        }
    }

    /// Apply one inbound frame: resolve a response, register or drop an observer, or
    /// dispatch a call to its home. Responses to calls (for frames carrying a request
    /// id) flow back through the sink, after any sends the handler itself made.
    pub fn deliver(&self, frame: Frame, cx: &mut App) {
        let Some(frame) = self.intercept_promised(frame) else {
            return;
        };
        let call = match frame {
            Frame::Response(response) => {
                self.deliver_response(response);
                return;
            }
            Frame::Subscribe { target, observer } => {
                let known = {
                    let mut state = self.inner.state.borrow_mut();
                    match state.homes.get_mut(&target) {
                        Some(home) => {
                            home.observers.push(observer);
                            true
                        }
                        None if target == ROOT_ADDRESS => {
                            // The other end's bootstrap outran ours; deliver once our
                            // root arrives.
                            state
                                .pending_root_frames
                                .push(Frame::Subscribe { target, observer });
                            return;
                        }
                        None => {
                            log::warn!("embedded_gpui: subscribe to unknown object {target}");
                            false
                        }
                    }
                };
                if known {
                    // A subscription is answered with an initial notify, so a new
                    // observer always fires at least once.
                    (self.inner.sink)(Frame::Call(Call {
                        target: observer,
                        request: None,
                        method: NOTIFY_EVENT.to_string(),
                        payload: Payload::empty(),
                        promised: None,
                    }));
                }
                return;
            }
            Frame::Release { target } => {
                if let Some(home) = self.inner.state.borrow_mut().homes.get_mut(&target) {
                    home.entity.release();
                    home.observers.clear();
                }
                return;
            }
            Frame::Call(call) => call,
        };

        let handler = {
            let mut state = self.inner.state.borrow_mut();
            let Some(home) = state.homes.get_mut(&call.target) else {
                if call.target == ROOT_ADDRESS {
                    // The other end's bootstrap outran ours; deliver once our root
                    // arrives.
                    state.pending_root_frames.push(Frame::Call(call));
                    return;
                }
                let id = call.target;
                drop(state);
                self.respond(call.request, Err(format!("call to unknown object {id}")));
                return;
            };
            // Straight to the object's one handler; how it interprets the name (a
            // `Methods` table, a wildcard, anything) is its business, never the
            // registry's.
            let handler = home.handler.clone();
            if let Some(promised) = call.promised {
                // Frames for the promised object wait here until it exists.
                state.pending_promises.entry(promised).or_default();
            }
            handler
        };
        // Refs in the payload resolve against this registry.
        let payload = call.payload.bound(self.downgrade());
        let outcome = match handler(&call.method, &payload, cx) {
            HandlerResponse::Ready(result) => result.map_err(|error| format!("{error:#}")),
            HandlerResponse::Pending(task) => {
                // The handler's work outlives this delivery; the response flows when
                // its task resolves.
                let objects = self.clone();
                let request = call.request;
                let promised = call.promised;
                cx.spawn(async move |cx| {
                    let outcome = task.await.map_err(|error| format!("{error:#}"));
                    if let Err(error) = &outcome {
                        log::error!("embedded_gpui: method call failed: {error}");
                    }
                    objects.respond(request, outcome.clone());
                    if let Some(promised) = promised {
                        cx.update(|cx| objects.resolve_promise(promised, &outcome, cx));
                    }
                })
                .detach();
                return;
            }
        };
        if let Err(error) = &outcome {
            log::error!("embedded_gpui: method call failed: {error}");
        }
        self.respond(call.request, outcome.clone());
        if let Some(promised) = call.promised {
            self.resolve_promise(promised, &outcome, cx);
        }
    }

    fn respond(&self, request: Option<u64>, outcome: Result<Payload, String>) {
        if let Some(request) = request {
            (self.inner.sink)(Frame::Response(Response { request, outcome }));
        }
    }

    /// Resolve the receipt waiting on an incoming response.
    fn deliver_response(&self, response: Response) {
        let sender = self
            .inner
            .state
            .borrow_mut()
            .pending_responses
            .remove(&response.request);
        let Some(sender) = sender else {
            log::warn!(
                "embedded_gpui: response for unknown request {}",
                response.request
            );
            return;
        };
        let outcome = response
            .outcome
            .map(|payload| payload.bound(self.downgrade()));
        sender.send(outcome).ok();
    }

    /// The other end is gone for good (the plugin trapped or was unloaded): fail every
    /// call still waiting on it so callers see an error instead of a hang.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn fail_pending(&self, reason: &str) {
        let pending = std::mem::take(&mut self.inner.state.borrow_mut().pending_responses);
        for (_, sender) in pending {
            sender.send(Err(reason.to_string())).ok();
        }
    }

    /// Flush queued capability releases (projections whose last `Remote` dropped) into
    /// `release` frames. Called from the boundary's pump, and before applying incoming
    /// work, so drops become observable promptly.
    pub fn drain_releases(&self) {
        loop {
            let released = std::mem::take(&mut *self.inner.releases.borrow_mut());
            if released.is_empty() {
                break;
            }
            for entity_id in released {
                self.release_id(entity_id);
            }
        }
    }

    /// Send `release` for a projection and forget it locally. The home end drops its
    /// strong handle; events stop flowing.
    fn release_id(&self, entity_id: u64) {
        let removed = self.inner.state.borrow_mut().projections.remove(&entity_id);
        let Some(projection) = removed else {
            return;
        };
        // The projection's hidden observer object goes with it; the home end forgets
        // its side on `release`.
        if let Some((_, observer_id)) = projection.signal {
            self.inner.state.borrow_mut().homes.remove(&observer_id);
        }
        (self.inner.sink)(Frame::Release { target: entity_id });
    }
}
