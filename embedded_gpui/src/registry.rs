//! The object registry: the entire object model, compiled identically into both ends of
//! the boundary.
//!
//! A registry knows exactly two things: *local* objects (homes -- entities whose state
//! lives here, keyed by ids this end allocates, with this end's root at its id 0) and
//! *remote* objects (projections of the other end's homes). There is no notion of host
//! or guest anywhere in this module: the two ends differ only in the configuration they
//! construct the registry with -- a transport sink that moves outgoing wire records, and
//! which half of the id namespace is theirs. Boundary creation assigns those; the object
//! model never mentions them again.
//!
//! Ids are random u64s, globally unique for practical purposes, so a ref is universally
//! applicable: payloads stay opaque (a caretaker can forward bytes verbatim and any refs
//! inside keep meaning the same objects), nothing is namespaced per end, and an id can
//! only be *known*, never guessed or enumerated — holding a ref is the authority. The
//! single reserved value is 0, "your root": a connection-local address (never an
//! identity in a payload) that each end answers with its own root object.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gpui::{AnyEntity, App, AppContext as _, Entity, Subscription};

use crate::{
    HandlerResponse, Interface, MethodHandler, Methods, NOTIFY_EVENT, RELEASE_METHOD, RawEvent,
    Ref, Remote, RemoteSignal, ResponseSender, SUBSCRIBE_METHOD, Shared, encode,
};

/// The reserved connection-local address meaning "the root object of whichever end you
/// send it to". Never minted as an object id and never meaningful inside a payload.
const ROOT_ADDRESS: u64 = 0;

/// A message toward a home on the other end, in wire-neutral form. The boundary layer
/// converts to its concrete wire types (wit-bindgen or wasmtime bindgen records).
pub(crate) struct WireMessage {
    pub entity_id: u64,
    pub request_id: Option<u64>,
    pub method: String,
    pub payload: Vec<u8>,
}

/// An event from a local home toward the other end's remotes.
pub(crate) struct WireEvent {
    pub entity_id: u64,
    pub name: String,
    pub payload: Vec<u8>,
}

/// The answer to a [`WireMessage`] that carried a `request_id`.
pub(crate) struct WireResponse {
    pub request_id: u64,
    pub outcome: Result<Vec<u8>, String>,
}

/// Everything the registry ever emits toward the other end.
pub(crate) enum WireOutgoing {
    Message(WireMessage),
    Event(WireEvent),
    Response(WireResponse),
}

/// How outgoing wire records leave this end: the single point of side-specific behavior,
/// supplied at boundary creation.
pub(crate) type WireSink = Box<dyn Fn(WireOutgoing)>;

/// A local object: an entity homed on this end, with its dynamic dispatch table.
struct HomeEntry {
    /// Interface name, kept purely as diagnostic metadata for error messages.
    type_name: &'static str,
    methods: HashMap<String, MethodHandler>,
    /// Whether the other end holds a live remote; events only flow while true.
    subscribed: bool,
    /// The registry keeps the entity alive until the other end releases it.
    strong: Option<AnyEntity>,
    /// The notify observation plus any typed-event forwarders wired at share time.
    _subscriptions: Vec<Subscription>,
}

/// A remote object: this end's projection of an entity homed on the other end.
struct Projection {
    /// Interface name as first connected, for a diagnostic on mismatched reconnects.
    type_name: &'static str,
    /// Where incoming events land. Created lazily by the first `observe`/`subscribe`
    /// (which is what makes `connect` context-free); events arriving before anyone
    /// listens are dropped, exactly as gpui drops events on entities with no
    /// subscribers.
    signal: Option<Entity<RemoteSignal>>,
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
    /// Messages addressed to this end's root before `share_root` ran. The other end's
    /// bootstrap may lawfully race ours (its `root()` remote subscribes immediately), so
    /// root traffic queues instead of failing; `share_root` drains it in order.
    pending_root_messages: Vec<WireMessage>,
}

struct Inner {
    sink: WireSink,
    /// Projections whose last `Remote` dropped; drained into `$release` sends.
    releases: Rc<RefCell<Vec<u64>>>,
    state: RefCell<State>,
}

/// A handle to one end's object registry. Clones share the registry; the boundary layer
/// owns one and the transports/guards of live remotes hold the others.
#[derive(Clone)]
pub(crate) struct Objects {
    inner: Rc<Inner>,
}

/// A non-owning handle for everything the registry itself stores (subscriptions, event
/// sinks) or that user entities may hold indefinitely (every `Remote`): a strong
/// capture there would cycle through `Inner` and keep every shared object alive
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
            }),
        }
    }

    /// Install `entity` as this end's root object (its id 0). The other end reaches it
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
        self.install::<S, T>(entity, methods, events, entity_id, cx);
        let queued = std::mem::take(&mut self.inner.state.borrow_mut().pending_root_messages);
        for message in queued {
            self.deliver_message(message, cx);
        }
    }

    /// Share a local entity, returning a capability reference to embed in message or
    /// event payloads. The registry holds the entity alive until the other end's last
    /// remote drops.
    pub fn share<S, T>(&self, entity: &Entity<T>, cx: &mut App) -> Ref<S>
    where
        S: Interface,
        T: Shared<S>,
    {
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let entity_id = self.reserve_local_id();
        let events = T::events(entity, self.event_sink(entity_id), cx);
        self.install::<S, T>(entity, methods, events, entity_id, cx);
        Ref::from_raw(entity_id)
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
        self.install::<S, T>(entity, methods, Vec::new(), entity_id, cx);
        Ref::from_raw(entity_id)
    }

    /// Attach to the other end's root object (the reserved address 0 means "your root"
    /// from either direction). Returns immediately; the other end queues root traffic
    /// until its root is installed.
    pub fn root<S: Interface>(&self) -> Remote<S> {
        self.connect_id(ROOT_ADDRESS)
    }

    /// Attach to an entity through a capability reference received in a payload.
    /// Connecting the same ref twice returns a handle to the same projection; when the
    /// last clone drops, the home end is told to release the entity. Context-free:
    /// connecting allocates nothing but a map entry.
    pub fn connect<S: Interface>(&self, reference: Ref<S>) -> Remote<S> {
        let entity_id = reference.entity_id();
        if self.inner.state.borrow().homes.contains_key(&entity_id) {
            log::warn!(
                "embedded_gpui: connecting a ref homed on this end (loopback) is not supported"
            );
        }
        self.connect_id(entity_id)
    }

    fn connect_id<S: Interface>(&self, entity_id: u64) -> Remote<S> {
        let existing = {
            let state = self.inner.state.borrow();
            state
                .projections
                .get(&entity_id)
                .and_then(|projection| Some((projection.guard.upgrade()?, projection.type_name)))
        };
        if let Some((guard, type_name)) = existing {
            if type_name != S::TYPE_NAME {
                log::error!(
                    "embedded_gpui: object {entity_id} connected as {} but already live as \
                     {type_name}",
                    S::TYPE_NAME
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
                type_name: S::TYPE_NAME,
                signal: None,
                guard: Rc::downgrade(&guard),
            },
        );
        self.send(entity_id, SUBSCRIBE_METHOD, Vec::new(), None);
        Remote::from_parts(self.downgrade(), entity_id, Some(guard))
    }

    /// The signal a projection's events land on, created on first demand (from
    /// `observe`/`subscribe`, which have a context). `None` if the projection is gone.
    pub(crate) fn signal_for(&self, entity_id: u64, cx: &mut App) -> Option<Entity<RemoteSignal>> {
        let existing = self
            .inner
            .state
            .borrow()
            .projections
            .get(&entity_id)?
            .signal
            .clone();
        if let Some(signal) = existing {
            return Some(signal);
        }
        let signal = cx.new(|_| RemoteSignal::new());
        self.inner
            .state
            .borrow_mut()
            .projections
            .get_mut(&entity_id)?
            .signal = Some(signal.clone());
        Some(signal)
    }

    pub(crate) fn send(
        &self,
        entity_id: u64,
        method: &str,
        payload: Vec<u8>,
        response: Option<ResponseSender>,
    ) {
        let message = {
            let mut state = self.inner.state.borrow_mut();
            if !state.projections.contains_key(&entity_id) {
                // Dropping `response` here resolves the caller's receipt with an error.
                log::warn!("embedded_gpui: send to released object {entity_id}");
                return;
            }
            let request_id = response.map(|sender| {
                state.next_request_id += 1;
                let request_id = state.next_request_id;
                state.pending_responses.insert(request_id, sender);
                request_id
            });
            WireMessage {
                entity_id,
                request_id,
                method: method.to_string(),
                payload,
            }
        };
        (self.inner.sink)(WireOutgoing::Message(message));
    }

    /// Mint a fresh object id: random, nonzero, and unused here. Randomness is what
    /// makes refs universally applicable (no per-end namespaces, so they pass through
    /// any number of hands unrewritten) and unguessable (an id can only be learned
    /// from a payload that carried it; enumeration is infeasible). Collisions with ids
    /// minted by the other end are birthday-bounded at ~2^-64 per pair; the loopback
    /// check in `connect` doubles as the tripwire.
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
        Rc::new(move |event: &str, payload: Vec<u8>, _cx: &mut App| {
            if let Some(objects) = objects.upgrade() {
                objects.emit_home_event(entity_id, event, payload);
            }
        })
    }

    fn install<S, T>(
        &self,
        entity: &Entity<T>,
        methods: Methods<S, T>,
        event_forwarders: Vec<Subscription>,
        entity_id: u64,
        cx: &mut App,
    ) where
        S: Interface,
        T: 'static,
    {
        let objects = self.downgrade();
        let mut subscriptions = vec![cx.observe(entity, move |_, _| {
            if let Some(objects) = objects.upgrade() {
                objects.emit_home_event(entity_id, NOTIFY_EVENT, Vec::new());
            }
        })];
        subscriptions.extend(event_forwarders);
        self.inner.state.borrow_mut().homes.insert(
            entity_id,
            HomeEntry {
                type_name: S::TYPE_NAME,
                methods: methods.into_map(),
                subscribed: false,
                strong: Some(entity.clone().into_any()),
                _subscriptions: subscriptions,
            },
        );
    }

    /// Send one event from a local home to the other end's remotes, if any are live.
    fn emit_home_event(&self, entity_id: u64, event: &str, payload: Vec<u8>) {
        let subscribed = self
            .inner
            .state
            .borrow()
            .homes
            .get(&entity_id)
            .is_some_and(|home| home.subscribed);
        if subscribed {
            (self.inner.sink)(WireOutgoing::Event(WireEvent {
                entity_id,
                name: event.to_string(),
                payload,
            }));
        }
    }

    /// Handle one incoming message to a local home: control methods inline, everything
    /// else through the dispatch table. Responses (for messages carrying a request id)
    /// flow back through the sink, after any sends the handler itself made.
    pub fn deliver_message(&self, message: WireMessage, cx: &mut App) {
        enum Dispatch {
            Handler(MethodHandler),
            Control,
            Unknown(String),
        }
        let dispatch = {
            let mut state = self.inner.state.borrow_mut();
            let Some(home) = state.homes.get_mut(&message.entity_id) else {
                if message.entity_id == ROOT_ADDRESS {
                    // The other end's bootstrap outran ours; deliver once our root
                    // arrives.
                    state.pending_root_messages.push(message);
                    return;
                }
                let id = message.entity_id;
                drop(state);
                self.respond(
                    message.request_id,
                    Err(format!("message for unknown object {id}")),
                );
                return;
            };
            match message.method.as_str() {
                SUBSCRIBE_METHOD => {
                    home.subscribed = true;
                    Dispatch::Control
                }
                RELEASE_METHOD => {
                    home.subscribed = false;
                    home.strong = None;
                    Dispatch::Control
                }
                method => home
                    .methods
                    .get(method)
                    .or_else(|| home.methods.get(crate::WILDCARD_METHOD))
                    .cloned()
                    .map(Dispatch::Handler)
                    .unwrap_or_else(|| {
                        Dispatch::Unknown(format!(
                            "object {} ({}) has no method {method:?}",
                            message.entity_id, home.type_name
                        ))
                    }),
            }
        };
        let outcome = match dispatch {
            Dispatch::Handler(handler) => {
                match handler(&message.method, &message.payload, cx) {
                    HandlerResponse::Ready(result) => result.map_err(|error| format!("{error:#}")),
                    HandlerResponse::Pending(task) => {
                        // The handler's work outlives this delivery; the response flows
                        // when its task resolves.
                        let objects = self.clone();
                        let request_id = message.request_id;
                        cx.spawn(async move |_| {
                            let outcome = task.await.map_err(|error| format!("{error:#}"));
                            if let Err(error) = &outcome {
                                log::error!("embedded_gpui: method call failed: {error}");
                            }
                            objects.respond(request_id, outcome);
                        })
                        .detach();
                        return;
                    }
                }
            }
            Dispatch::Control => {
                if message.method == SUBSCRIBE_METHOD {
                    // A subscription is answered with an initial notify, so a new
                    // remote's observers always fire at least once.
                    self.emit_home_event(message.entity_id, NOTIFY_EVENT, Vec::new());
                }
                encode(&()).map_err(|error| format!("{error:#}"))
            }
            Dispatch::Unknown(error) => Err(error),
        };
        if let Err(error) = &outcome {
            log::error!("embedded_gpui: method call failed: {error}");
        }
        self.respond(message.request_id, outcome);
    }

    fn respond(&self, request_id: Option<u64>, outcome: Result<Vec<u8>, String>) {
        if let Some(request_id) = request_id {
            (self.inner.sink)(WireOutgoing::Response(WireResponse {
                request_id,
                outcome,
            }));
        }
    }

    /// Deliver an incoming event from the other end to this end's remotes.
    pub fn deliver_event(&self, event: WireEvent, cx: &mut App) {
        // Clone the signal out so the registry borrow is released before user code
        // (observers and subscribers) runs; observers may re-enter this module via sends.
        let signal = {
            let state = self.inner.state.borrow();
            let Some(projection) = state.projections.get(&event.entity_id) else {
                log::warn!(
                    "embedded_gpui: event for unknown object {}",
                    event.entity_id
                );
                return;
            };
            // No signal means nobody has observed or subscribed yet; dropping the
            // event matches gpui's behavior for entities with no subscribers.
            let Some(signal) = projection.signal.clone() else {
                return;
            };
            signal
        };
        if event.name == NOTIFY_EVENT {
            signal.update(cx, |_, cx| cx.notify());
        } else {
            signal.update(cx, |_, cx| {
                cx.emit(RawEvent {
                    name: event.name,
                    payload: event.payload,
                })
            });
        }
    }

    /// Resolve the receipt waiting on an incoming response.
    pub fn deliver_response(&self, response: WireResponse) {
        let sender = self
            .inner
            .state
            .borrow_mut()
            .pending_responses
            .remove(&response.request_id);
        let Some(sender) = sender else {
            log::warn!(
                "embedded_gpui: response for unknown request {}",
                response.request_id
            );
            return;
        };
        sender.send(response.outcome).ok();
    }

    /// Flush queued capability releases (projections whose last `Remote` dropped) into
    /// `$release` sends. Called from the boundary's pump, and before applying incoming
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

    /// Send `$release` for a projection and forget it locally. The home end drops its
    /// strong handle; events stop flowing.
    fn release_id(&self, entity_id: u64) {
        let existed = self
            .inner
            .state
            .borrow_mut()
            .projections
            .remove(&entity_id)
            .is_some();
        if existed {
            (self.inner.sink)(WireOutgoing::Message(WireMessage {
                entity_id,
                request_id: None,
                method: RELEASE_METHOD.to_string(),
                payload: Vec::new(),
            }));
        }
    }
}
