//! Schemas for the demo: the interfaces both ends compile against. Each
//! `#[shared_interface]` is one name for the whole thing — hold a `Remote<CounterApi>`,
//! reference a `SharedRef<CommandApi>`, implement `#[shared] impl CounterApi for ...`.
//!
//! The two root interfaces are the entire bootstrap: each end installs its root object
//! at its id 0, and every other capability here is reached by calling a root method
//! that returns a ref. No names, no registries — discovery *is* the root schema.

use embedded_gpui::{SharedRef, shared_data, shared_interface};

/// The host's root object: everything the plugin can reach on the host.
#[shared_interface]
pub trait DemoHost {
    /// The shared click counter, homed on the host.
    fn counter(&mut self, cx: &mut gpui::Context<Self>) -> SharedRef<CounterApi>;

    /// The workspace service the plugin drives (toasts, accent color).
    fn workspace(&mut self, cx: &mut gpui::Context<Self>) -> SharedRef<WorkspaceApi>;
}

/// The plugin's root object: everything the host can reach in the plugin. The methods
/// are lazy factories — the entities are created on first call and cached, so both ends
/// converge on the same objects.
#[shared_interface]
pub trait DemoPlugin {
    /// The wasm input line's text, mirrored natively by the host.
    fn typed_text(&mut self, cx: &mut gpui::Context<Self>) -> SharedRef<TextApi>;

    /// The plugin's command palette, rendered natively by the host.
    fn palette(&mut self, cx: &mut gpui::Context<Self>) -> SharedRef<PaletteApi>;
}

/// The click counter homed on the HOST: the wasm views call `increment`, mirror
/// `clicks` for rendering, and hear `Milestone` events.
#[shared_interface(events = [Milestone])]
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

/// The wasm input line's text, homed in the PLUGIN and mirrored natively by the host.
#[shared_interface]
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

/// The registry the host renders natively: plugin-homed, mirrored by the host.
#[shared_interface]
pub trait PaletteApi {
    fn commands(&mut self, cx: &mut gpui::Context<Self>) -> Vec<PaletteEntry>;
}

/// A command the host can invoke on the plugin; discovered through [`PaletteApi`],
/// addressed only by ref.
#[shared_interface]
pub trait CommandApi {
    fn invoke(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

/// The mirror image of [`CommandApi`]: a service homed on the HOST that the plugin
/// drives. Same macro, other direction — the host implements the trait, and the
/// plugin's `Remote<WorkspaceApi>` gets the typed caller methods.
#[shared_interface]
pub trait WorkspaceApi {
    fn show_toast(&mut self, message: String, cx: &mut gpui::Context<Self>) -> String;
    fn set_accent(&mut self, hue: f32, cx: &mut gpui::Context<Self>) -> String;
}
