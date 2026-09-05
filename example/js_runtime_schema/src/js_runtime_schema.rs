//! The root interface of the `js_runtime` component: how a host loads JavaScript into
//! it. Every other method on that root is answered by the script itself
//! (`plugin.root.<method>`), so this schema is deliberately tiny.

use embedded_gpui::interface;

#[interface]
pub trait JsRuntimeApi {
    /// Evaluate `source` in the runtime's context. Calling it again replaces whatever
    /// the previous script installed on `plugin.root`: hot reload is a second `load`.
    /// Returns the error message if evaluation failed.
    fn load(&mut self, source: String, cx: &mut gpui::Context<Self>) -> Option<String>;
}
