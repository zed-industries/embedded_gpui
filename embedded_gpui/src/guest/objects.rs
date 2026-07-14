//! Guest-side boundary for the object registry: a thread-local [`Objects`] whose sink is
//! the WIT imports, plus free-function wrappers. The object model itself lives in
//! `registry` and is identical on both ends; this file only moves bytes.

use crate::registry::{Objects, WireCall, WireMessage, WireOutgoing, WireResponse};
use crate::wit;
use embedded_gpui::{Interface, Methods, Ref, Remote, Shared};
use gpui::{App, AsyncApp, Entity};

thread_local! {
    static OBJECTS: Objects = Objects::new(Box::new(deliver_outgoing));
}

fn deliver_outgoing(outgoing: WireOutgoing) {
    match outgoing {
        WireOutgoing::Message(message) => wit::send_object_message(&message_to_wire(message)),
        WireOutgoing::Response(response) => wit::send_object_response(&wit::ObjectResponse {
            request_id: response.request_id,
            outcome: response.outcome,
        }),
    }
}

/// Registry frames -> wit-bindgen wire variants, and back. Purely structural.
fn message_to_wire(message: WireMessage) -> wit::ObjectMessage {
    match message {
        WireMessage::Call(call) => wit::ObjectMessage::Call(wit::ObjectCall {
            entity_id: call.entity_id,
            request_id: call.request_id,
            method: call.method,
            payload: call.payload,
        }),
        WireMessage::Subscribe {
            entity_id,
            observer_id,
        } => wit::ObjectMessage::Subscribe(wit::ObjectSubscribe {
            entity_id,
            observer_id,
        }),
        WireMessage::Release { entity_id } => {
            wit::ObjectMessage::Release(wit::ObjectRelease { entity_id })
        }
    }
}

fn message_from_wire(message: wit::ObjectMessage) -> WireMessage {
    match message {
        wit::ObjectMessage::Call(call) => WireMessage::Call(WireCall {
            entity_id: call.entity_id,
            request_id: call.request_id,
            method: call.method,
            payload: call.payload,
        }),
        wit::ObjectMessage::Subscribe(subscribe) => WireMessage::Subscribe {
            entity_id: subscribe.entity_id,
            observer_id: subscribe.observer_id,
        },
        wit::ObjectMessage::Release(release) => WireMessage::Release {
            entity_id: release.entity_id,
        },
    }
}

fn objects() -> Objects {
    OBJECTS.with(|objects| objects.clone())
}

/// Install `entity` as this end's root object (its id 0): the single capability the
/// other end starts from, whose typed methods return everything else. Call it from
/// [`Plugin::new`](crate::Plugin::new). Root traffic that arrived first is queued and
/// delivered in order once the root exists.
pub fn share_root<S, T>(entity: &Entity<T>, cx: &mut App)
where
    S: Interface,
    T: Shared<S>,
{
    objects().share_root(entity, cx);
}

/// Share a local entity, returning a capability reference to embed in message or event
/// payloads. The registry holds the entity alive until the other end's last remote
/// drops.
pub fn share<S, T>(entity: &Entity<T>, cx: &mut App) -> Ref<S>
where
    S: Interface,
    T: Shared<S>,
{
    objects().share(entity, cx)
}

/// [`share`] with a closure-registered dispatch table instead of a schema interface:
/// the dynamic escape hatch. `cx.notify` still crosses; typed events are not wired
/// (implement [`Shared`](embedded_gpui::Shared) manually if you need both).
pub fn share_with<S, T>(
    entity: &Entity<T>,
    register: impl FnOnce(&mut Methods<S, T>),
    cx: &mut App,
) -> Ref<S>
where
    S: Interface,
    T: 'static,
{
    objects().share_with(entity, register, cx)
}

/// Attach to the other end's root object: the single typed capability everything
/// starts from. Returns immediately (root traffic queues until the other end's root is
/// installed), so the whole bootstrap is synchronous.
pub fn root<S: Interface>() -> Remote<S> {
    objects().root()
}

/// Attach to an entity through a capability reference received in a payload. Connecting
/// the same ref twice returns a handle to the same projection; when the last clone
/// drops, the home end is told to release the entity.
pub fn connect<S: Interface>(reference: Ref<S>) -> Remote<S> {
    objects().connect(reference)
}

/// Flush queued capability releases; called from the guest's pump so drops become
/// observable to the other end promptly.
pub(crate) fn drain_releases() {
    objects().drain_releases();
}

pub(crate) fn message_delivered(message: wit::ObjectMessage, cx: &mut AsyncApp) {
    let objects = objects();
    cx.update(|cx| objects.deliver_message(message_from_wire(message), cx));
}

pub(crate) fn response_delivered(response: wit::ObjectResponse) {
    objects().deliver_response(WireResponse {
        request_id: response.request_id,
        outcome: response.outcome,
    });
}
