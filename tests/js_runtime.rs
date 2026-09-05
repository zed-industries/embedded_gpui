//! End-to-end tests for the JavaScript runtime: the `js_runtime` component (QuickJS
//! inside wasm) loaded into a wasmtime store, driven from GPUI's deterministic test
//! executor, running `tests/js_plugin.js` against a host root and a host surface.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use embedded_gpui::surface::{
    Geometry, Modifiers, MouseButton, MouseButtonEvent, MouseEvent, Point, ViewApiCaller as _,
};
use embedded_gpui::{
    PluginHost, PluginHostHandle as _, PluginInstance, PluginOptions, Ref, Remote, Surface, decode,
    encode, shared,
};
use gpui::{AppContext as _, Context, Entity, TestAppContext};
use js_runtime_schema::{JsRuntimeApi, JsRuntimeApiCaller as _};
use serde::{Deserialize, Serialize};
use test_schema::TestHost;

const PLUGIN_SOURCE: &str = include_str!("js_plugin.js");

/// Builds the JS runtime component once per process and returns its path.
fn js_runtime_path() -> PathBuf {
    use std::sync::Once;
    static BUILD: Once = Once::new();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let runtime_dir = manifest_dir.join("../example/js_runtime");
    BUILD.call_once(|| {
        // Blocking is fine here: tests build their fixture once, up front.
        #[allow(clippy::disallowed_methods)]
        let output = std::process::Command::new("cargo")
            .args(["build", "--target", "wasm32-wasip2"])
            .current_dir(&runtime_dir)
            .output()
            .expect("failed to spawn cargo to build js_runtime");
        assert!(
            output.status.success(),
            "building js_runtime failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
    runtime_dir.join("target/wasm32-wasip2/debug/js_runtime.wasm")
}

struct HostRoot {
    pings: u32,
}

#[shared]
impl TestHost for HostRoot {
    fn ping(&mut self, message: String, cx: &mut Context<Self>) -> String {
        self.pings += 1;
        cx.notify();
        format!("pong: {message}")
    }
}

fn settle(cx: &mut TestAppContext) {
    for _ in 0..5 {
        cx.executor().run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
    }
    cx.executor().run_until_parked();
}

/// A host with the runtime loaded and the test script evaluated.
fn setup(cx: &mut TestAppContext) -> (Entity<PluginHost>, Remote<JsRuntimeApi>, Entity<HostRoot>) {
    let path = js_runtime_path();
    let instance = cx.update(|_| {
        PluginInstance::new(
            &path,
            PluginOptions::new(Arc::new(gpui::NoopTextSystem::new())),
        )
        .expect("failed to instantiate js_runtime")
    });
    let host = cx.new(|cx| PluginHost::new(instance, cx));
    let root = cx.new(|_| HostRoot { pings: 0 });
    cx.update(|cx| host.share_root(&root, cx));
    let runtime = cx.update(|cx| host.root::<JsRuntimeApi>(cx));
    let loaded = cx.update(|cx| runtime.load(PLUGIN_SOURCE.to_string(), cx));
    settle(cx);
    assert_eq!(
        loaded.now_or_never().expect("load resolved").expect("load"),
        None,
        "the script must evaluate without errors"
    );
    (host, runtime, root)
}

use futures::FutureExt as _;

/// Call a method the *script* defines: the runtime's root forwards it to `plugin.root`.
macro_rules! call_js {
    ($runtime:expr, $cx:expr, $method:literal, $args:expr => $response:ty) => {{
        let receipt = $cx.update(|cx| {
            $runtime
                .call_raw($method, encode(&$args).expect("encode"), cx)
                .decoded::<$response>()
        });
        settle($cx);
        receipt.await
    }};
}

#[derive(Serialize)]
struct Greet {
    name: String,
}

#[derive(Serialize)]
struct EchoPing {
    message: String,
}

#[derive(Serialize)]
struct Relay {
    target: Ref<TestHost>,
}

#[derive(Deserialize)]
struct Relayed {
    forwarded: String,
    again: Ref<TestHost>,
}

#[derive(Serialize)]
struct Mount {
    surface: Ref<embedded_gpui::SurfaceApi>,
}

#[derive(Serialize)]
struct NoArgs {}

#[gpui::test]
async fn test_script_answers_calls(cx: &mut TestAppContext) {
    let (_host, runtime, root) = setup(cx);

    // Synchronous answer from JS.
    let greeting = call_js!(runtime, cx, "greet", Greet { name: "gpui".into() } => String);
    assert_eq!(greeting.expect("greet"), "hello, gpui");

    // JS calls back into the host's root and answers when that promise settles.
    let echoed =
        call_js!(runtime, cx, "echo_ping", EchoPing { message: "round trip".into() } => String);
    assert_eq!(echoed.expect("echo_ping"), "pong: round trip");
    assert_eq!(root.read_with(cx, |root, _| root.pings), 1);

    // Unknown methods are the script's error, reported as a failed call.
    let missing = call_js!(runtime, cx, "nonsense", NoArgs {} => String);
    let error = missing.expect_err("missing method must fail");
    assert!(
        error.to_string().contains("no method"),
        "unexpected error: {error:#}"
    );
}

#[gpui::test]
async fn test_refs_cross_into_and_out_of_javascript(cx: &mut TestAppContext) {
    let (host, runtime, root) = setup(cx);

    // A ref in the arguments arrives in JS as a remote (the `$ref` shape needs no
    // schema); the script calls it and hands it back in its answer, where it decodes to
    // a ref to the same object.
    let root_ref = cx.update(|cx| host.share(&root, cx));
    let relayed = call_js!(runtime, cx, "relay", Relay { target: root_ref.clone() } => Relayed);
    let relayed = relayed.expect("relay");
    assert_eq!(relayed.forwarded, "pong: via js");
    assert_eq!(relayed.again.entity_id(), root_ref.entity_id());
}

#[gpui::test]
async fn test_script_renders_a_view_and_receives_input(cx: &mut TestAppContext) {
    let (host, runtime, _root) = setup(cx);

    let surface = cx.new(Surface::new);
    let surface_ref = cx.update(|cx| host.share(&surface, cx));
    let mounted = call_js!(runtime, cx, "mount", Mount { surface: surface_ref } => String);
    assert_eq!(mounted.expect("mount"), "mounted");

    // The script's view attached itself to our surface; give it geometry and it draws.
    let view = surface
        .read_with(cx, |surface, _| surface.view().cloned())
        .expect("the script attached a view");
    cx.update(|cx| {
        view.resize(
            Geometry {
                width: 200.,
                height: 100.,
                scale_factor: 1.,
            },
            cx,
        )
    });
    settle(cx);
    assert!(
        surface.read_with(cx, |surface, _| surface.has_scene()),
        "the JS view rendered a display list"
    );

    // A click travels host -> view object -> GPUI dispatch in the guest -> the handler
    // id in the tree -> the script's closure, which re-renders.
    let click = MouseButtonEvent {
        button: MouseButton::Left,
        position: Point { x: 20., y: 20. },
        modifiers: Modifiers::default(),
        click_count: 1,
    };
    cx.update(|cx| {
        view.mouse(MouseEvent::Down(click.clone()), cx);
        view.mouse(MouseEvent::Up(click), cx);
    });
    settle(cx);
    let clicks = call_js!(runtime, cx, "clicks", NoArgs {} => u32);
    assert_eq!(clicks.expect("clicks"), 1);
}

#[gpui::test]
async fn test_reload_starts_clean(cx: &mut TestAppContext) {
    let (host, runtime, _root) = setup(cx);
    let surface = cx.new(Surface::new);
    let surface_ref = cx.update(|cx| host.share(&surface, cx));
    call_js!(runtime, cx, "mount", Mount { surface: surface_ref } => String).expect("mount");
    let first_view = surface
        .read_with(cx, |surface, _| surface.view().cloned())
        .expect("first view");

    // Reloading evaluates the script in a fresh context: state resets and the old view's
    // window is gone, so its object no longer answers.
    let reloaded = cx.update(|cx| runtime.load(PLUGIN_SOURCE.to_string(), cx));
    settle(cx);
    assert_eq!(reloaded.await.expect("reload"), None);
    let clicks = call_js!(runtime, cx, "clicks", NoArgs {} => u32);
    assert_eq!(clicks.expect("clicks after reload"), 0);

    // Mounting again attaches a new view to the same surface.
    let surface_ref = cx.update(|cx| host.share(&surface, cx));
    call_js!(runtime, cx, "mount", Mount { surface: surface_ref } => String).expect("remount");
    let second_view = surface
        .read_with(cx, |surface, _| surface.view().cloned())
        .expect("second view");
    assert_ne!(
        first_view.reference().entity_id(),
        second_view.reference().entity_id()
    );
    let payload = decode::<String>(&encode(&"still decoding").expect("encode")).expect("decode");
    assert_eq!(payload, "still decoding");
}
