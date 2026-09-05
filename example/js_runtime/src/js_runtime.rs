//! The JavaScript runtime as a loadable component. All of it lives in
//! `embedded_gpui_js`; this crate exists because a component needs a `cdylib` to be
//! built from, and to give the demo's `plugins/counter.js` a home.

embedded_gpui::register_plugin!(embedded_gpui_js::JsPlugin);
