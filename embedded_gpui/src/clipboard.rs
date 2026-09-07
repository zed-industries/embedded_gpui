//! The host clipboard as an object. The host homes a [`Clipboard`](crate::Clipboard)
//! entity and hands its ref to a plugin through the host's own root schema (or does
//! not, or wraps it first — a `Revocable` around it is a clipboard loan). A plugin that
//! holds the ref calls `read` and `write` like any other capability, and subscribes to
//! [`ClipboardChanged`] to keep the guest platform's synchronous
//! `read_from_clipboard` fresh: the host refreshes the object on every modified
//! key-down it forwards, so the event reaches the guest ahead of the paste that reads it
//! (events are frames, and queries queue behind frames).

use crate::{data, interface};

/// A clipboard: text in, text out, and a change event.
#[interface(events = [ClipboardChanged])]
pub trait ClipboardApi {
    fn read(&mut self, cx: &mut gpui::Context<Self>) -> Option<String>;
    fn write(&mut self, text: String, cx: &mut gpui::Context<Self>);
}

/// The clipboard's text changed (as far as the host looked).
#[data]
#[derive(PartialEq, Eq)]
pub struct ClipboardChanged {
    pub text: Option<String>,
}
