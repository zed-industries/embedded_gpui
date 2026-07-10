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
//! Ids are canonical per connection rather than perspective-relative (bit 63 marks which
//! end homes the object) so that payloads stay opaque: a caretaker can forward bytes
//! verbatim and any refs inside keep meaning the same objects.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gpui::{AnyEntity, App, AppContext as _, Entity, Subscription};

use crate::{
    HandlerResponse, MethodHandler, Methods, NOTIFY_EVENT, RELEASE_METHOD, RawSharedEvent, Remote,
    RemoteSignal, ResponseSender, SUBSCRIBE_METHOD, Shared, SharedRef, SharedSpec, Transport,
    encode,
};

/// The id-namespace bit distinguishing the two ends of a connection. Which end gets it
/// is boundary configuration: the wasm embedding assigns [`Side::A`] to the embedder and
/// [`Side::B`] to the component.
const SIDE_B_BIT: u64 = 1 << 63;

/// One end of a connection, as assigned when the boundary is created. Purely an
/// id-namespace choice; the two sides are otherwise identical. Each build constructs
/// exactly one variant (the other end constructs the other), hence the dead-code allow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub(crate) enum Side {
    A,
    B,
}

impl Side {
    fn local_bit(self) -> u64 {
        match self {
            Side::A => 0,
            Side::B => SIDE_B_BIT,
        }
    }

    fn remote_bit(self) -> u64 {
        match self {
            Side::A => SIDE_B_BIT,
            Side::B => 0,
        }
    }
}

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
struct Home {
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
    /// Where incoming events land; every `Remote` for this entity holds it.
    signal: Entity<RemoteSignal>,
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
    next_local_id: u64,
    homes: HashMap<u64, Home>,
    projections: HashMap<u64, Projection>,
    next_request_id: u64,
    pending_responses: HashMap<u64, ResponseSender>,
    /// Messages addressed to this end's root before `share_root` ran. The other end's
    /// bootstrap may lawfully race ours (its `remote_root` subscribes immediately), so
    /// root traffic queues instead of failing; `share_root` drains it in order.
    pending_root_messages: Vec<WireMessage>,
}

struct Inner {
    side: Side,
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
/// sinks) or that user entities may hold indefinitely (transports): a strong capture
/// there would cycle through `Inner` and keep every shared entity alive forever.
#[derive(Clone)]
struct WeakObjects {
    inner: Weak<Inner>,
}

impl WeakObjects {
    fn upgrade(&self) -> Option<Objects> {
        Some(Objects {
            inner: self.inner.upgrade()?,
        })
    }
}

impl Objects {
    fn downgrade(&self) -> WeakObjects {
        WeakObjects {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub fn new(side: Side, sink: WireSink) -> Self {
        Self {
            inner: Rc::new(Inner {
                side,
                sink,
                releases: Rc::default(),
                state: RefCell::new(State::default()),
            }),
        }
    }

    fn local_root_id(&self) -> u64 {
        self.inner.side.local_bit()
    }

    fn remote_root_id(&self) -> u64 {
        self.inner.side.remote_bit()
    }

    /// Install `entity` as this end's root object (its id 0). The other end reaches it
    /// with `remote_root`; everything else it can reach is a method of this object
    /// returning refs. Root traffic that arrived first was queued and is delivered
    /// now, in order, so the two bootstraps may race freely.
    pub fn share_root<S, T>(&self, entity: &Entity<T>, cx: &mut App)
    where
        S: SharedSpec,
        T: Shared<S>,
    {
        let entity_id = self.local_root_id();
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
    pub fn share<S, T>(&self, entity: &Entity<T>, cx: &mut App) -> SharedRef<S>
    where
        S: SharedSpec,
        T: Shared<S>,
    {
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let entity_id = self.reserve_local_id();
        let events = T::events(entity, self.event_sink(entity_id), cx);
        self.install::<S, T>(entity, methods, events, entity_id, cx);
        SharedRef::from_raw(entity_id)
    }

    /// [`Objects::share`] with a closure-registered dispatch table instead of a schema
    /// interface: the dynamic escape hatch. `cx.notify` still crosses; typed events are
    /// not wired (implement [`Shared`] manually if you need both).
    pub fn share_with<S, T>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut App,
    ) -> SharedRef<S>
    where
        S: SharedSpec,
        T: 'static,
    {
        let mut methods = Methods::new(entity.downgrade());
        register(&mut methods);
        let entity_id = self.reserve_local_id();
        self.install::<S, T>(entity, methods, Vec::new(), entity_id, cx);
        SharedRef::from_raw(entity_id)
    }

    /// Attach to the other end's root object (its id 0). Returns immediately: the
    /// remote's sends are ordered after this end's own bootstrap, so they arrive once
    /// the other end has installed its root.
    pub fn remote_root<S: SharedSpec>(&self, cx: &mut App) -> Remote<S> {
        self.connect_id(self.remote_root_id(), cx)
    }

    /// Attach to an entity through a capability reference received in a payload.
    /// Connecting the same ref twice returns a handle to the same projection; when the
    /// last clone drops, the home end is told to release the entity.
    pub fn connect<S: SharedSpec>(&self, reference: SharedRef<S>, cx: &mut App) -> Remote<S> {
        let entity_id = reference.entity_id();
        if entity_id & SIDE_B_BIT == self.inner.side.local_bit() & SIDE_B_BIT {
            log::warn!(
                "embedded_gpui: connecting a ref homed on this end (loopback) is not supported"
            );
        }
        self.connect_id(entity_id, cx)
    }

    fn connect_id<S: SharedSpec>(&self, entity_id: u64, cx: &mut App) -> Remote<S> {
        let existing = {
            let state = self.inner.state.borrow();
            state.projections.get(&entity_id).and_then(|projection| {
                Some((
                    projection.signal.clone(),
                    projection.guard.upgrade()?,
                    projection.type_name,
                ))
            })
        };
        if let Some((signal, guard, type_name)) = existing {
            if type_name != S::TYPE_NAME {
                log::error!(
                    "embedded_gpui: object {entity_id} connected as {} but already live as \
                     {type_name}",
                    S::TYPE_NAME
                );
            }
            return Remote::from_parts(signal, self.transport(entity_id), Some(guard));
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
        let signal = cx.new(|_| RemoteSignal::new());
        let guard = Rc::new(ReleaseGuard {
            entity_id,
            queue: self.inner.releases.clone(),
        });
        self.inner.state.borrow_mut().projections.insert(
            entity_id,
            Projection {
                type_name: S::TYPE_NAME,
                signal: signal.clone(),
                guard: Rc::downgrade(&guard),
            },
        );
        self.send(entity_id, SUBSCRIBE_METHOD, Vec::new(), None);
        Remote::from_parts(signal, self.transport(entity_id), Some(guard))
    }

    /// The transport handed to every `Remote`: bytes go straight to the sink, so sends
    /// are safe from anywhere (they never re-enter the boundary entity). Holds the
    /// registry weakly: remotes can outlive the boundary they came from, and a send
    /// after teardown resolves the receipt with an error.
    fn transport(&self, entity_id: u64) -> Rc<Transport> {
        let objects = self.downgrade();
        Rc::new(
            move |method: &str, payload: Vec<u8>, response: Option<ResponseSender>, _cx| {
                let Some(objects) = objects.upgrade() else {
                    // Dropping `response` resolves the caller's receipt with an error.
                    log::warn!("embedded_gpui: send after the boundary was torn down");
                    return;
                };
                objects.send(entity_id, method, payload, response);
            },
        )
    }

    fn send(
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

    fn reserve_local_id(&self) -> u64 {
        let mut state = self.inner.state.borrow_mut();
        state.next_local_id += 1;
        self.inner.side.local_bit() | state.next_local_id
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
        S: SharedSpec,
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
            Home {
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
                if message.entity_id == self.local_root_id() {
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
                                log::error!("embedded_gpui: shared message failed: {error}");
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
            log::error!("embedded_gpui: shared message failed: {error}");
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
        let signal = self
            .inner
            .state
            .borrow()
            .projections
            .get(&event.entity_id)
            .map(|projection| projection.signal.clone());
        let Some(signal) = signal else {
            log::warn!(
                "embedded_gpui: event for unknown object {}",
                event.entity_id
            );
            return;
        };
        if event.name == NOTIFY_EVENT {
            signal.update(cx, |_, cx| cx.notify());
        } else {
            signal.update(cx, |_, cx| {
                cx.emit(RawSharedEvent {
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
