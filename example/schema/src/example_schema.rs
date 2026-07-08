//! Schemas for the demo: a click counter homed on the host and projected into the plugin,
//! and the plugin's input-line text homed in the guest and projected into the host.

use embedded_gpui::{SharedRef, shared_interface};

embedded_gpui::shared_schema! {
    entity CounterSpec as "gpui-embedded.demo.counter" {
        snapshot CounterSnapshot { clicks: u32 }
        message "increment" Increment { by: u32 } -> u32
    }
}

embedded_gpui::shared_schema! {
    entity TextSpec as "gpui-embedded.demo.text" {
        snapshot TextSnapshot { text: String }
    }
}

embedded_gpui::shared_schema! {
    entity PaletteSpec as "demo.palette" {
        snapshot PaletteSnapshot { commands: Vec<PaletteEntry> }
    }
}

/// One entry in the plugin's published command palette: a label the host can render
/// natively, and the capability to invoke the command. The ref is the authority —
/// holding the snapshot is holding the right to run every command in it.
#[derive(Clone, Debug, embedded_gpui::serde::Serialize, embedded_gpui::serde::Deserialize)]
#[serde(crate = "embedded_gpui::serde")]
pub struct PaletteEntry {
    pub label: String,
    pub command: SharedRef<CommandSpec>,
}

#[derive(Clone, Debug, embedded_gpui::serde::Serialize, embedded_gpui::serde::Deserialize)]
#[serde(crate = "embedded_gpui::serde")]
pub struct CommandSnapshot {
    pub label: String,
    pub detail: String,
}

/// A command the host can invoke on the plugin. Defined with [`shared_interface`],
/// which generates the spec, the `Invoke` message, the `CommandApiCaller` extension
/// trait for remotes on either side, and `register_command_api`.
#[shared_interface(spec = CommandSpec, type_name = "demo.command", snapshot = CommandSnapshot)]
pub trait CommandApi {
    fn invoke(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

#[derive(Clone, Debug, embedded_gpui::serde::Serialize, embedded_gpui::serde::Deserialize)]
#[serde(crate = "embedded_gpui::serde")]
pub struct WorkspaceSnapshot {
    pub accent_hue: f32,
    pub last_toast: Option<String>,
}

/// The mirror image of [`CommandApi`]: a service homed on the HOST that the plugin
/// drives. The same macro generates the same shape in the other direction — the
/// host implements the trait, and the guest's `Remote<WorkspaceSpec>` gets the
/// typed caller methods.
#[shared_interface(spec = WorkspaceSpec, type_name = "demo.workspace", snapshot = WorkspaceSnapshot)]
pub trait WorkspaceApi {
    fn show_toast(&mut self, message: String, cx: &mut gpui::Context<Self>) -> String;
    fn set_accent(&mut self, hue: f32, cx: &mut gpui::Context<Self>) -> String;
}
