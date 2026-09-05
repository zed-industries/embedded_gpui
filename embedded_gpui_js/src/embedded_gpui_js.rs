//! JavaScript plugins for `embedded_gpui`.
//!
//! An optional layer on top of the object model, in two halves that compile where they
//! are needed:
//!
//! - **everywhere**: [`JsRuntimeApi`], the root interface a host uses to load scripts
//!   into a runtime component, and [`typescript`], which renders any interface schemas
//!   as `.d.ts` so script authors type-check against the same schema the Rust side
//!   compiled;
//! - **`wasm32` only**: the runtime — [`JsPlugin`], a ready-made [`Plugin`] embedding
//!   QuickJS, and [`JsRoot`], the entity behind its root, for plugins that want to host
//!   scripts alongside their own Rust objects.
//!
//! A component crate that wants to *be* the JS runtime is one line:
//!
//! ```ignore
//! embedded_gpui::register_plugin!(embedded_gpui_js::JsPlugin);
//! ```
//!
//! A JS plugin is then a directory with an `index.js`: the host mounts it with
//! `PluginOptions::with_plugin_dir` (a generic grant, not a JavaScript one), the runtime
//! runs `/plugin/index.js` and reloads it when it changes, and the host addresses the
//! plugin's root exactly as it would a Rust plugin's. `load(source)` remains for tools
//! and tests that push scripts directly.
//!
//! Scripts see the object model directly: `host` is the host's root, remotes are proxies
//! whose methods return promises (`counter.increment({ by: 1 })`) and which `observe`
//! the home's notifies, refs cross as `{"$ref": n}` so no schema is consulted at
//! runtime, and UI is a data tree (`div`, `text`) handed to `view.render` after
//! `plugin.openView(surface)`. The JS half of that contract is `src/prelude.js`.
//!
//! [`Plugin`]: embedded_gpui::Plugin

use embedded_gpui::interface;

pub mod typescript;

#[cfg(target_arch = "wasm32")]
mod runtime;
#[cfg(target_arch = "wasm32")]
pub use runtime::{ENTRY_POINT, JsPlugin, JsRoot};

/// The root interface of a JS runtime component: how a host loads JavaScript into it.
/// Every other method on that root is answered by the script itself
/// (`plugin.root.<method>`), so this schema is deliberately tiny.
#[interface]
pub trait JsRuntimeApi {
    /// Evaluate `source` in a fresh context, tearing down whatever the previous script
    /// built (its views, observers, and held remotes) first: hot reload is a second
    /// `load`. Returns the error message if evaluation failed.
    fn load(&mut self, source: String, cx: &mut gpui::Context<Self>) -> Option<String>;
}
