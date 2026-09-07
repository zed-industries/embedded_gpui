//! Guest-side boundary for the object registry: a thread-local [`Objects`] whose sink is
//! the outbound frame queue of the current turn, plus free-function wrappers. The object
//! model itself lives in `registry` and is identical on both ends; this file only moves
//! frames.

use std::cell::RefCell;

use crate::registry::{Call, Frame, Objects, Response};
use crate::wit;
use embedded_gpui::{Interface, Methods, Payload, Ref, Registry, Remote, Shared};
use gpui::{App, Entity};

thread_local! {
    static OBJECTS: Objects = Objects::new(Box::new(|frame| {
        OUTBOUND.with(|outbound| outbound.borrow_mut().push(frame_to_wire(frame)))
    }));
    /// Frames toward the host, collected until the turn returns.
    static OUTBOUND: RefCell<Vec<wit::Frame>> = const { RefCell::new(Vec::new()) };
    /// Display lists toward the host, collected until the turn returns.
    static SCENES: RefCell<Vec<wit::Scene>> = const { RefCell::new(Vec::new()) };
    static OVERLAYS: RefCell<Vec<wit::Scene>> = const { RefCell::new(Vec::new()) };
}

/// Registry frames -> wit-bindgen wire records, and back. Purely structural.
fn frame_to_wire(frame: Frame) -> wit::Frame {
    match frame {
        Frame::Call(call) => wit::Frame::Call(wit::Call {
            target: call.target,
            request: call.request,
            method: call.method,
            payload: call.payload.bytes,
            refs: call.payload.refs,
        }),
        Frame::Response(response) => {
            let (outcome, refs) = match response.outcome {
                Ok(payload) => (Ok(payload.bytes), payload.refs),
                Err(error) => (Err(error), Vec::new()),
            };
            wit::Frame::Response(wit::Response {
                request: response.request,
                outcome,
                refs,
            })
        }
        Frame::Subscribe { target, observer } => {
            wit::Frame::Subscribe(wit::Subscribe { target, observer })
        }
        Frame::Release { target } => wit::Frame::Release(wit::Release { target }),
    }
}

fn frame_from_wire(frame: wit::Frame) -> Frame {
    match frame {
        wit::Frame::Call(call) => Frame::Call(Call {
            target: call.target,
            request: call.request,
            method: call.method,
            payload: Payload::from_parts(call.payload, call.refs),
        }),
        wit::Frame::Response(response) => Frame::Response(Response {
            request: response.request,
            outcome: response
                .outcome
                .map(|bytes| Payload::from_parts(bytes, response.refs)),
        }),
        wit::Frame::Subscribe(subscribe) => Frame::Subscribe {
            target: subscribe.target,
            observer: subscribe.observer,
        },
        wit::Frame::Release(release) => Frame::Release {
            target: release.target,
        },
    }
}

fn objects() -> Objects {
    OBJECTS.with(|objects| objects.clone())
}

/// This end's registry, for sharing and connecting from anywhere.
pub fn registry() -> Registry {
    Registry::new(objects().downgrade())
}

/// Install `entity` as this end's root object (address 0): the single capability the
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

/// Flush queued capability releases; called from the guest's pump so drops become
/// observable to the other end promptly.
pub(crate) fn drain_releases() {
    objects().drain_releases();
}

/// Apply one turn's inbound frames, in order.
pub(crate) fn deliver(inbound: Vec<wit::Frame>, cx: &mut App) {
    let objects = objects();
    for frame in inbound {
        objects.deliver(frame_from_wire(frame), cx);
    }
}

/// Queue a rendered display list for the host.
pub(crate) fn push_scene(surface: u64, list: wit::DisplayList) {
    SCENES.with(|scenes| scenes.borrow_mut().push(wit::Scene { surface, list }));
}

/// Queue a surface's overlay display list for the host.
pub(crate) fn push_overlay(surface: u64, list: wit::DisplayList) {
    OVERLAYS.with(|overlays| overlays.borrow_mut().push(wit::Scene { surface, list }));
}

/// Everything queued since the last turn, as the `tick` export returns it.
pub(crate) fn take_turn(wake_after_ms: Option<u32>) -> wit::Turn {
    wit::Turn {
        frames: OUTBOUND.with(|outbound| std::mem::take(&mut *outbound.borrow_mut())),
        scenes: SCENES.with(|scenes| std::mem::take(&mut *scenes.borrow_mut())),
        overlays: OVERLAYS.with(|overlays| std::mem::take(&mut *overlays.borrow_mut())),
        wake_after_ms,
    }
}
