//! Schemas for the demo: the interfaces both sides compile against. Each
//! `#[shared_interface]` is one name for the whole thing — hold a `Remote<CounterApi>`,
//! reference a `SharedRef<CommandApi>`, implement `#[shared] impl CounterApi for ...`.

use embedded_gpui::{SharedRef, shared_data, shared_interface};

/// The click counter homed on the HOST: the wasm views call `increment`, mirror
/// `clicks` for rendering, and hear `Milestone` events.
#[shared_interface("demo.counter", events = [Milestone])]
pub trait CounterApi {
    fn increment(&mut self, by: u32, cx: &mut gpui::Context<Self>) -> u32;
    fn clicks(&mut self, cx: &mut gpui::Context<Self>) -> u32;
}

/// Emitted by the counter's home every fifth click: an ordinary GPUI event (`cx.emit`)
/// crossing the boundary to `Remote::subscribe`.
#[shared_data]
pub struct Milestone {
    pub clicks: u32,
}

/// The wasm input line's text, homed in the GUEST and mirrored natively by the host.
#[shared_interface("demo.text")]
pub trait TextApi {
    fn text(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

/// One entry in the plugin's published command palette: a label the host can render
/// natively, and the capability to invoke the command. The ref is the authority —
/// holding the list is holding the right to run every command in it.
#[shared_data]
pub struct PaletteEntry {
    pub label: String,
    pub command: SharedRef<CommandApi>,
}

/// The registry the host renders natively: guest-homed, mirrored by the host.
#[shared_interface("demo.palette")]
pub trait PaletteApi {
    fn commands(&mut self, cx: &mut gpui::Context<Self>) -> Vec<PaletteEntry>;
}

/// A command the host can invoke on the plugin; discovered through [`PaletteApi`],
/// addressed only by ref.
#[shared_interface("demo.command")]
pub trait CommandApi {
    fn invoke(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

/// The mirror image of [`CommandApi`]: a service homed on the HOST that the plugin
/// drives. Same macro, other direction — the host implements the trait, and the guest's
/// `Remote<WorkspaceApi>` gets the typed caller methods.
#[shared_interface("demo.workspace")]
pub trait WorkspaceApi {
    fn show_toast(&mut self, message: String, cx: &mut gpui::Context<Self>) -> String;
    fn set_accent(&mut self, hue: f32, cx: &mut gpui::Context<Self>) -> String;
}
