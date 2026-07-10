//! Guest-side shared entities: homes for guest-owned entities, and the registry behind
//! [`Remote`]s to host-homed entities. See the "Shared entities" section of
//! `wit/plugin.wit` and DESIGN.md.

use crate::wit;
use embedded_gpui::{
    EventSink, HandlerResponse, MethodHandler, NOTIFY_EVENT, RELEASE_METHOD, RawSharedEvent,
    RemoteSignal, ResponseSender, SUBSCRIBE_METHOD, Shared, SharedSpec, encode,
};
use gpui::{AnyEntity, App, AppContext as _, AsyncApp, Entity, Subscription};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

pub use embedded_gpui::{Methods, Receipt, Remote, SharedCaller, SharedRef};

use crate::GUEST_HOME_BIT;

struct PendingSend {
    request_id: Option<u64>,
    method: String,
    payload: Vec<u8>,
}

struct ProjectionEntry {
    type_name: &'static str,
    entity_id: Option<u64>,
    /// Where incoming events land; every `Remote` for this entity holds it.
    signal: Entity<RemoteSignal>,
    /// Messages sent before the home side's announcement arrived; flushed in order, which
    /// is what makes sends to a not-yet-resolved entity pipeline correctly.
    pending_sends: Vec<PendingSend>,
    /// Live only while some `Remote` from `connect` still holds the projection; used to
    /// hand the same guard back when the same ref is connected twice.
    guard: Weak<ReleaseGuard>,
}

/// Dropping the last `Remote` for a ref-derived projection releases the capability:
/// the home side drops its strong handle and events stop flowing.
struct ReleaseGuard {
    name: String,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        dispatch_outgoing(&self.name, RELEASE_METHOD, Vec::new(), None);
        REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            if let Some(entry) = registry.projections_by_name.remove(&self.name)
                && let Some(entity_id) = entry.entity_id
            {
                registry.names_by_entity_id.remove(&entity_id);
            }
        });
    }
}

struct HomeEntry {
    methods: HashMap<String, MethodHandler>,
    /// Whether the host holds a live remote; events only flow while true.
    subscribed: bool,
    /// Anonymous shares keep their entity alive until released; named shares borrow.
    strong: Option<AnyEntity>,
    /// The notify observation plus any typed-event forwarders wired at share time.
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct Registry {
    projections_by_name: HashMap<String, ProjectionEntry>,
    names_by_entity_id: HashMap<u64, String>,
    homes: HashMap<u64, HomeEntry>,
    next_home_id: u64,
    next_request_id: u64,
    pending_responses: HashMap<u64, ResponseSender>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Attach to the shared entity bound to `name` on the host. Returns immediately; sends
/// queue (in order) until the host's announcement arrives, then flush.
pub fn remote<S: SharedSpec>(name: impl Into<String>, cx: &mut App) -> Remote<S> {
    let name = name.into();
    let signal = cx.new(|_| RemoteSignal::new());
    REGISTRY.with(|registry| {
        registry.borrow_mut().projections_by_name.insert(
            name.clone(),
            ProjectionEntry {
                type_name: S::TYPE_NAME,
                entity_id: None,
                signal: signal.clone(),
                pending_sends: Vec::new(),
                guard: Weak::new(),
            },
        );
    });
    dispatch_outgoing(&name, SUBSCRIBE_METHOD, Vec::new(), None);
    Remote::from_parts(signal, remote_transport(name), None)
}

/// Attach to a shared entity through a capability reference received in a payload. No
/// name is involved: the ref's id addresses the entity directly. Connecting the same ref
/// twice returns a handle to the same projection; when the last clone drops, the home is
/// told to release the entity.
pub fn connect<S: SharedSpec>(reference: SharedRef<S>, cx: &mut App) -> Remote<S> {
    let entity_id = reference.entity_id();
    let name = format!("#{entity_id}");

    let existing = REGISTRY.with(|registry| {
        let registry = registry.borrow();
        registry.projections_by_name.get(&name).and_then(|entry| {
            Some((
                entry.signal.clone(),
                entry.guard.upgrade()?,
                entry.type_name,
            ))
        })
    });
    if let Some((signal, guard, type_name)) = existing {
        if type_name != S::TYPE_NAME {
            log::error!(
                "embedded_gpui: ref {entity_id} connected as {} but already live as {type_name}",
                S::TYPE_NAME
            );
        }
        return Remote::from_parts(signal, remote_transport(name), Some(guard));
    }

    let signal = cx.new(|_| RemoteSignal::new());
    let guard = Rc::new(ReleaseGuard { name: name.clone() });
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.projections_by_name.insert(
            name.clone(),
            ProjectionEntry {
                type_name: S::TYPE_NAME,
                entity_id: Some(entity_id),
                signal: signal.clone(),
                pending_sends: Vec::new(),
                guard: Rc::downgrade(&guard),
            },
        );
        registry.names_by_entity_id.insert(entity_id, name.clone());
    });
    dispatch_outgoing(&name, SUBSCRIBE_METHOD, Vec::new(), None);
    Remote::from_parts(signal, remote_transport(name), Some(guard))
}

/// The guest's transport: the registry is thread-local, so moving bytes toward the host
/// needs no deferral and no context.
fn remote_transport(name: String) -> Rc<embedded_gpui::Transport> {
    Rc::new(
        move |method: &str, payload: Vec<u8>, response: Option<ResponseSender>, _cx| {
            dispatch_outgoing(&name, method, payload, response);
        },
    )
}

fn dispatch_outgoing(name: &str, method: &str, payload: Vec<u8>, response: Option<ResponseSender>) {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if !registry.projections_by_name.contains_key(name) {
            // Dropping `response` here resolves the caller's receipt with an error.
            log::warn!("embedded_gpui: send to unknown shared entity {name:?}");
            return;
        }
        let request_id = response.map(|sender| {
            registry.next_request_id += 1;
            let request_id = registry.next_request_id;
            registry.pending_responses.insert(request_id, sender);
            request_id
        });
        let Some(entry) = registry.projections_by_name.get_mut(name) else {
            return;
        };
        if let Some(entity_id) = entry.entity_id {
            wit::send_shared_message(&wit::SharedMessage {
                entity_id,
                request_id,
                method: method.to_string(),
                payload,
            });
        } else {
            entry.pending_sends.push(PendingSend {
                request_id,
                method: method.to_string(),
                payload,
            });
        }
    });
}

pub(crate) fn response_delivered(response: wit::SharedResponse) {
    let sender = REGISTRY.with(|registry| {
        registry
            .borrow_mut()
            .pending_responses
            .remove(&response.request_id)
    });
    let Some(sender) = sender else {
        log::warn!(
            "embedded_gpui: response for unknown request {}",
            response.request_id
        );
        return;
    };
    sender.send(response.outcome).ok();
}

/// Share a guest entity with the host under a well-known name (a mount the host attaches
/// to with `PluginHost::remote`). The guest becomes the home: the entity's
/// `#[shared]` methods answer host messages, and its `cx.notify` / declared
/// `cx.emit` events reach every host remote.
pub fn share<S, T>(entity: &Entity<T>, name: impl Into<String>, cx: &mut App)
where
    S: SharedSpec,
    T: Shared<S>,
{
    let mut methods = Methods::new(entity.downgrade());
    T::methods(&mut methods);
    let entity_id = reserve_home_id();
    let events = T::events(entity, home_event_sink(entity_id), cx);
    install_home::<S, T>(entity, methods, events, None, entity_id, cx);
    wit::announce_shared_entity(&wit::SharedEntityAnnouncement {
        entity_id,
        type_name: S::TYPE_NAME.to_string(),
        name: name.into(),
    });
}

/// The dynamic escape hatch beneath [`share`]: register method handlers with a closure
/// instead of a schema interface. `cx.notify` still crosses; typed events are not wired
/// (implement [`Shared`] manually if you need both).
pub fn share_with<S, T>(
    entity: &Entity<T>,
    name: impl Into<String>,
    register: impl FnOnce(&mut Methods<S, T>),
    cx: &mut App,
) where
    S: SharedSpec,
    T: 'static,
{
    let mut methods = Methods::new(entity.downgrade());
    register(&mut methods);
    let entity_id = reserve_home_id();
    install_home::<S, T>(entity, methods, Vec::new(), None, entity_id, cx);
    wit::announce_shared_entity(&wit::SharedEntityAnnouncement {
        entity_id,
        type_name: S::TYPE_NAME.to_string(),
        name: name.into(),
    });
}

/// Share a guest entity anonymously, returning a capability reference to embed in message
/// or event payloads. The home holds a strong handle to the entity until the reference is
/// released.
pub fn share_anonymous<S, T>(entity: &Entity<T>, cx: &mut App) -> SharedRef<S>
where
    S: SharedSpec,
    T: Shared<S>,
{
    let mut methods = Methods::new(entity.downgrade());
    T::methods(&mut methods);
    let entity_id = reserve_home_id();
    let events = T::events(entity, home_event_sink(entity_id), cx);
    install_home::<S, T>(
        entity,
        methods,
        events,
        Some(entity.clone().into_any()),
        entity_id,
        cx,
    );
    SharedRef::from_raw(entity_id)
}

/// [`share_anonymous`] with a closure-registered dispatch table; see [`share_with`].
pub fn share_anonymous_with<S, T>(
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
    let entity_id = reserve_home_id();
    install_home::<S, T>(
        entity,
        methods,
        Vec::new(),
        Some(entity.clone().into_any()),
        entity_id,
        cx,
    );
    SharedRef::from_raw(entity_id)
}

fn reserve_home_id() -> u64 {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.next_home_id += 1;
        GUEST_HOME_BIT | registry.next_home_id
    })
}

fn install_home<S, T>(
    entity: &Entity<T>,
    methods: Methods<S, T>,
    event_forwarders: Vec<Subscription>,
    strong: Option<AnyEntity>,
    entity_id: u64,
    cx: &mut App,
) where
    S: SharedSpec,
    T: 'static,
{
    let mut subscriptions = vec![cx.observe(entity, move |_, _| {
        emit_home_event(entity_id, NOTIFY_EVENT, Vec::new());
    })];
    subscriptions.extend(event_forwarders);
    REGISTRY.with(|registry| {
        registry.borrow_mut().homes.insert(
            entity_id,
            HomeEntry {
                methods: methods.into_map(),
                subscribed: false,
                strong,
                _subscriptions: subscriptions,
            },
        );
    });
}

/// The sink handed to schema-generated event wiring: it moves a home's typed events onto
/// the wire.
fn home_event_sink(entity_id: u64) -> EventSink {
    Rc::new(move |event: &str, payload: Vec<u8>, _cx: &mut App| {
        emit_home_event(entity_id, event, payload);
    })
}

/// Send one event from a guest home to the host's remotes, if the host holds any.
fn emit_home_event(entity_id: u64, event: &str, payload: Vec<u8>) {
    let subscribed = REGISTRY.with(|registry| {
        registry
            .borrow()
            .homes
            .get(&entity_id)
            .is_some_and(|home| home.subscribed)
    });
    if subscribed {
        wit::emit_shared_event(&wit::SharedEvent {
            entity_id,
            name: event.to_string(),
            payload,
        });
    }
}

pub(crate) fn entity_announced(announcement: wit::SharedEntityAnnouncement) {
    let flushed = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let Some(entry) = registry.projections_by_name.get_mut(&announcement.name) else {
            log::info!(
                "embedded_gpui: no local projection for shared entity {:?} ({})",
                announcement.name,
                announcement.type_name
            );
            return Vec::new();
        };
        if entry.type_name != announcement.type_name {
            log::error!(
                "embedded_gpui: shared entity {:?} is a {} on the host but bound as {} here",
                announcement.name,
                announcement.type_name,
                entry.type_name
            );
            return Vec::new();
        }
        entry.entity_id = Some(announcement.entity_id);
        registry
            .names_by_entity_id
            .insert(announcement.entity_id, announcement.name.clone());
        let entry = registry
            .projections_by_name
            .get_mut(&announcement.name)
            .expect("looked up above");
        std::mem::take(&mut entry.pending_sends)
            .into_iter()
            .map(|send| (announcement.entity_id, send))
            .collect::<Vec<_>>()
    });
    for (entity_id, send) in flushed {
        wit::send_shared_message(&wit::SharedMessage {
            entity_id,
            request_id: send.request_id,
            method: send.method,
            payload: send.payload,
        });
    }
}

pub(crate) fn event_delivered(event: wit::SharedEvent, cx: &mut AsyncApp) {
    // Clone the signal out so the registry borrow is released before user code (observers
    // and subscribers) runs; observers may re-enter this module via sends.
    let signal = REGISTRY.with(|registry| {
        let registry = registry.borrow();
        let name = registry.names_by_entity_id.get(&event.entity_id)?;
        registry
            .projections_by_name
            .get(name)
            .map(|entry| entry.signal.clone())
    });
    let Some(signal) = signal else {
        log::warn!(
            "embedded_gpui: event for unknown shared entity {}",
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

pub(crate) fn message_delivered(message: wit::SharedMessage, cx: &mut AsyncApp) {
    enum Dispatch {
        Handler(MethodHandler),
        Control,
        Unknown,
    }
    let dispatch = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let Some(home) = registry.homes.get_mut(&message.entity_id) else {
            return Dispatch::Unknown;
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
            _ => home
                .methods
                .get(&message.method)
                .or_else(|| home.methods.get(embedded_gpui::WILDCARD_METHOD))
                .cloned()
                .map(Dispatch::Handler)
                .unwrap_or(Dispatch::Unknown),
        }
    });
    let outcome = match dispatch {
        Dispatch::Handler(handler) => {
            match cx.update(|cx| handler(&message.method, &message.payload, cx)) {
                HandlerResponse::Ready(result) => result.map_err(|error| format!("{error:#}")),
                HandlerResponse::Pending(task) => {
                    // The handler's work continues after this delivery returns; the
                    // response flows when its task resolves.
                    let request_id = message.request_id;
                    cx.spawn(async move |_| {
                        let outcome = task.await.map_err(|error| format!("{error:#}"));
                        if let Err(error) = &outcome {
                            log::error!("embedded_gpui: shared message failed: {error}");
                        }
                        respond(request_id, outcome);
                    })
                    .detach();
                    return;
                }
            }
        }
        Dispatch::Control => {
            if message.method == SUBSCRIBE_METHOD {
                // A subscription is answered with an initial notify, so a new remote's
                // observers always fire at least once.
                emit_home_event(message.entity_id, NOTIFY_EVENT, Vec::new());
            }
            encode(&()).map_err(|error| format!("{error:#}"))
        }
        Dispatch::Unknown => Err(format!(
            "no handler for shared method {:?} on entity {}",
            message.method, message.entity_id
        )),
    };
    if let Err(error) = &outcome {
        log::error!("embedded_gpui: shared message failed: {error}");
    }
    respond(message.request_id, outcome);
}

fn respond(request_id: Option<u64>, outcome: Result<Vec<u8>, String>) {
    if let Some(request_id) = request_id {
        wit::send_shared_response(&wit::SharedResponse {
            request_id,
            outcome,
        });
    }
}
