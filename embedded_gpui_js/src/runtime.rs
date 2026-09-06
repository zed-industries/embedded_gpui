//! The QuickJS runtime: an ordinary plugin whose root object answers `load(source)`
//! itself and forwards every other method to the script's `plugin.root`; remotes reach
//! JS as proxies whose methods return promises; UI is a data tree the script renders and
//! this module turns into GPUI elements on a host surface.
//!
//! JS never holds a GPUI context. While a script runs, everything it asks of the outside
//! world is queued as an [`Op`]; once the run returns, Rust drains the queue with the
//! real `App`. The JS half of the contract lives in `prelude.js`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use crate::JsRuntimeApi;
use anyhow::{Context as _, anyhow};
use embedded_gpui::log;
use embedded_gpui::surface::SurfaceApi;
use embedded_gpui::{
    Methods, Opaque, Payload, Plugin, Ref, Remote, Shared, ViewHandle, WILDCARD_METHOD, open_view,
    registry, share_root,
};
use futures::channel::oneshot;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, Styled as _, Subscription, Task, Window, div, px, rgb,
};
use rquickjs::{CatchResultExt as _, Ctx, Function, Persistent, Runtime, Value};
use serde::Deserialize;

const PRELUDE: &str = include_str!("prelude.js");

/// The script a JS plugin ships: the host mounts the plugin's directory at `/plugin`
/// (`PluginOptions::with_plugin_dir`), and this is what the runtime runs from it.
pub const ENTRY_POINT: &str = "/plugin/index.js";

/// How often the runtime checks whether the entry point changed on disk.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// A ready-made plugin whose root is a [`JsRoot`]: the whole JavaScript runtime as one
/// component. Register it with `embedded_gpui::register_plugin!(embedded_gpui_js::JsPlugin)`.
///
/// On start it runs [`ENTRY_POINT`] if the host mounted a plugin directory, and reloads
/// it whenever the file changes — so a JS plugin is a directory with an `index.js`, and
/// the host never learns that JavaScript is involved. Without a plugin directory the
/// runtime waits for `load(source)` instead.
pub struct JsPlugin {
    root: Entity<JsRoot>,
    _watcher: Option<Task<()>>,
}

impl Plugin for JsPlugin {
    fn new(cx: &mut App) -> Self {
        let root = cx.new(|_| JsRoot::new());
        share_root(&root, cx);
        let watcher = match std::fs::metadata(ENTRY_POINT) {
            Ok(_) => Some(Self::run_from_disk(root.clone(), cx)),
            Err(_) => {
                log::info!("embedded_gpui_js: no {ENTRY_POINT}; waiting for load()");
                None
            }
        };
        Self {
            root,
            _watcher: watcher,
        }
    }
}

impl JsPlugin {
    /// The root, for plugins that embed the runtime and want to load scripts themselves.
    pub fn root(&self) -> &Entity<JsRoot> {
        &self.root
    }

    /// Run the entry point now, then poll its modification time and rerun on change.
    fn run_from_disk(root: Entity<JsRoot>, cx: &mut App) -> Task<()> {
        let mut last_modified = Self::load_entry_point(&root, cx);
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(WATCH_INTERVAL).await;
                let modified = std::fs::metadata(ENTRY_POINT)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                if modified.is_some() && modified != last_modified {
                    cx.update(|cx| last_modified = Self::load_entry_point(&root, cx));
                }
            }
        })
    }

    fn load_entry_point(root: &Entity<JsRoot>, cx: &mut App) -> Option<std::time::SystemTime> {
        let modified = std::fs::metadata(ENTRY_POINT)
            .and_then(|metadata| metadata.modified())
            .ok();
        match std::fs::read_to_string(ENTRY_POINT) {
            Ok(source) => {
                if let Some(error) = root.update(cx, |root, cx| root.load(source, cx)) {
                    log::error!("embedded_gpui_js: {ENTRY_POINT} failed to load: {error}");
                }
            }
            Err(error) => log::error!("embedded_gpui_js: reading {ENTRY_POINT}: {error}"),
        }
        modified
    }
}

// ---------------------------------------------------------------------------------
// The root object.
// ---------------------------------------------------------------------------------

/// The entity behind a JS runtime's root: it implements
/// [`JsRuntimeApi`](crate::JsRuntimeApi) (`load`) and answers every other method from
/// the loaded script. A plugin that wants to host scripts next to its own objects can
/// create one and share it like any entity; there is one runtime per component.
pub struct JsRoot;

impl JsRoot {
    /// Create the root and install a fresh (empty) runtime.
    pub fn new() -> Self {
        JsState::install();
        Self
    }
}

impl Default for JsRoot {
    fn default() -> Self {
        Self::new()
    }
}

impl JsRoot {
    /// Evaluate a script in a fresh context. Whatever the previous script built —
    /// views, observers, the remotes it held — is torn down first, so a reload is a
    /// clean start rather than a second layer. Every call the host has made to this
    /// root is then replayed to the new script, in order, so the views the host mounted
    /// come back without the host knowing anything changed. Returns the error message
    /// on failure.
    pub fn load(&mut self, source: String, cx: &mut Context<Self>) -> Option<String> {
        JsState::reset(cx);
        let evaluated = run_js(cx, |ctx| {
            ctx.eval::<(), _>(source.as_bytes())
                .catch(ctx)
                .map_err(|error| error.to_string())
        });
        if let Err(error) = evaluated {
            return Some(error);
        }
        let history = JsState::with(|state| state.history.clone());
        for (method, payload) in history {
            dispatch(&method, &payload, cx).detach();
        }
        None
    }
}

impl Shared<JsRuntimeApi> for JsRoot {
    fn methods(methods: &mut Methods<JsRuntimeApi, Self>) {
        JsRuntimeApi::register_load::<Self>(methods, Self::load);
        // Everything else is the script's business.
        methods.on_async(WILDCARD_METHOD, |_, method, payload, cx| {
            dispatch_to_js(method, payload, cx)
        });
    }
}

/// Hand a host call to `plugin.root[method]` and resolve when the script answers. The
/// call is remembered for replay on reload.
fn dispatch_to_js(method: &str, payload: &Payload, cx: &mut App) -> Task<anyhow::Result<Payload>> {
    JsState::with(|state| state.history.push((method.to_string(), payload.clone())));
    dispatch(method, payload, cx)
}

fn dispatch(method: &str, payload: &Payload, cx: &mut App) -> Task<anyhow::Result<Payload>> {
    let (sender, receiver) = oneshot::channel();
    let request = JsState::with(|state| {
        state.next_request += 1;
        state.pending.insert(state.next_request, sender);
        state.next_request
    });
    let method = method.to_string();
    let json = String::from_utf8_lossy(&payload.bytes).into_owned();
    let refs = ref_strings(&payload.refs);
    let dispatched = run_js(cx, |ctx| {
        prelude_fn(ctx, "dispatch")
            .and_then(|dispatch: Function| {
                dispatch.call::<_, ()>((request as f64, method, json, refs))
            })
            .catch(ctx)
            .map_err(|error| error.to_string())
    });
    cx.spawn(async move |_| {
        dispatched.map_err(|error| anyhow!("{error}"))?;
        receiver
            .await
            .context("the script never answered")?
            .map_err(|error| anyhow!("{error}"))
    })
}

// ---------------------------------------------------------------------------------
// The runtime state and the operation queue.
// ---------------------------------------------------------------------------------

type JsFunction = Persistent<Function<'static>>;

/// One thing a script asked for while running; applied by [`drain_ops`] afterwards,
/// when a GPUI context is available.
enum Op {
    Call {
        target: u64,
        method: String,
        payload: Payload,
        resolve: JsFunction,
        reject: JsFunction,
    },
    Observe {
        target: u64,
        callback: JsFunction,
    },
    OpenView {
        surface: u64,
        key: u32,
    },
    Render {
        key: u32,
        tree: String,
    },
    Respond {
        request: u64,
        outcome: Result<Payload, String>,
    },
}

struct JsState {
    runtime: Runtime,
    context: rquickjs::Context,
    ops: Vec<Op>,
    /// Remotes the script holds, kept connected for as long as the script is loaded.
    remotes: HashMap<u64, Remote<Opaque>>,
    subscriptions: Vec<Subscription>,
    /// The script's open views.
    views: HashMap<u32, ViewHandle<JsView>>,
    pending: HashMap<u64, oneshot::Sender<Result<Payload, String>>>,
    next_request: u64,
    next_view: u32,
    /// Every call the host made to the root, in order: what a reloaded script is
    /// replayed. Survives `reset`, since it is about the host, not the script.
    history: Vec<(String, Payload)>,
}

thread_local! {
    static STATE: RefCell<Option<JsState>> = const { RefCell::new(None) };
}

impl JsState {
    fn install() {
        Self::install_with(Vec::new());
    }

    fn install_with(history: Vec<(String, Payload)>) {
        let runtime = Runtime::new().expect("quickjs runtime");
        let context = rquickjs::Context::full(&runtime).expect("quickjs context");
        context.with(|ctx| {
            install_rt(&ctx).expect("installing __rt");
            ctx.eval::<(), _>(PRELUDE.as_bytes())
                .catch(&ctx)
                .expect("prelude");
        });
        STATE.with(|slot| {
            *slot.borrow_mut() = Some(JsState {
                runtime,
                context,
                ops: Vec::new(),
                remotes: HashMap::new(),
                subscriptions: Vec::new(),
                views: HashMap::new(),
                pending: HashMap::new(),
                next_request: 0,
                next_view: 0,
                history,
            })
        });
    }

    fn with<R>(f: impl FnOnce(&mut JsState) -> R) -> R {
        STATE.with(|slot| f(slot.borrow_mut().as_mut().expect("js runtime installed")))
    }

    /// Discard the running script and everything it built, and start a fresh context.
    /// Windows are removed (the host's surfaces then show nothing until a new view
    /// attaches), observers are cancelled, and held remotes are released. Requests the
    /// old script never answered fail.
    fn reset(cx: &mut App) {
        let previous = STATE.with(|slot| slot.borrow_mut().take());
        let mut history = Vec::new();
        if let Some(previous) = previous {
            for handle in previous.views.into_values() {
                handle.close(cx);
            }
            for (_, sender) in previous.pending {
                sender.send(Err("the script was reloaded".to_string())).ok();
            }
            drop(previous.subscriptions);
            drop(previous.remotes);
            history = previous.history;
        }
        Self::install_with(history);
    }

    fn push(op: Op) {
        Self::with(|state| state.ops.push(op));
    }
}

/// Run `f` against the JS context, then run every job it queued (promise reactions)
/// and apply every operation the script asked for.
fn run_js<R>(cx: &mut App, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
    let (runtime, context) = JsState::with(|state| (state.runtime.clone(), state.context.clone()));
    let result = context.with(|ctx| f(&ctx));
    while let Ok(true) = runtime.execute_pending_job() {}
    drain_ops(cx);
    result
}

fn drain_ops(cx: &mut App) {
    loop {
        let ops = JsState::with(|state| std::mem::take(&mut state.ops));
        if ops.is_empty() {
            return;
        }
        for op in ops {
            apply_op(op, cx);
        }
    }
}

fn remote_for(target: u64) -> Remote<Opaque> {
    JsState::with(|state| {
        state
            .remotes
            .entry(target)
            .or_insert_with(|| registry().connect::<Opaque>(target))
            .clone()
    })
}

fn apply_op(op: Op, cx: &mut App) {
    match op {
        Op::Call {
            target,
            method,
            payload,
            resolve,
            reject,
        } => {
            let receipt = remote_for(target).call_raw(&method, payload, cx);
            cx.spawn(async move |cx| {
                let outcome = receipt.await;
                cx.update(|cx| {
                    run_js(cx, |ctx| {
                        let settled = match outcome {
                            Ok(payload) => {
                                let json = String::from_utf8_lossy(&payload.bytes).into_owned();
                                let refs = ref_strings(&payload.refs);
                                prelude_fn(ctx, "decode").and_then(|decode: Function| {
                                    let value: Value = decode.call((json, refs))?;
                                    resolve.clone().restore(ctx)?.call::<_, ()>((value,))
                                })
                            }
                            Err(error) => reject
                                .clone()
                                .restore(ctx)
                                .and_then(|reject| reject.call::<_, ()>((error.to_string(),))),
                        };
                        if let Err(error) = settled.catch(ctx) {
                            log::error!("js_runtime: settling a call failed: {error}");
                        }
                    })
                });
            })
            .detach();
        }
        Op::Observe { target, callback } => {
            let subscription = remote_for(target).observe(cx, move |cx| {
                run_js(cx, |ctx| {
                    let called = callback
                        .clone()
                        .restore(ctx)
                        .and_then(|callback| callback.call::<_, ()>(()));
                    if let Err(error) = called.catch(ctx) {
                        log::error!("js_runtime: observer failed: {error}");
                    }
                })
            });
            JsState::with(|state| state.subscriptions.push(subscription));
        }
        Op::OpenView { surface, key } => {
            let surface: Ref<SurfaceApi> = registry().reference(surface);
            let view = cx.new(|_| JsView { tree: None });
            let root = view.clone();
            match open_view(surface, cx, move |_, _| root) {
                Ok(handle) => JsState::with(|state| {
                    state.views.insert(key, handle);
                }),
                Err(error) => log::error!("js_runtime: open_view failed: {error:#}"),
            }
        }
        Op::Render { key, tree } => {
            let view =
                JsState::with(|state| state.views.get(&key).map(|handle| handle.entity().clone()));
            let Some(view) = view else {
                log::warn!("js_runtime: render for unknown view {key}");
                return;
            };
            match serde_json::from_str::<Node>(&tree) {
                Ok(tree) => view.update(cx, |view, cx| {
                    view.tree = Some(tree);
                    cx.notify();
                }),
                Err(error) => log::error!("js_runtime: malformed UI tree: {error}"),
            }
        }
        Op::Respond { request, outcome } => {
            if let Some(sender) = JsState::with(|state| state.pending.remove(&request)) {
                sender.send(outcome).ok();
            }
        }
    }
}

/// The functions the prelude is written against, as `__rt`. Each one only queues.
fn install_rt<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<()> {
    let rt = rquickjs::Object::new(ctx.clone())?;
    rt.set(
        "call",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>,
             target: String,
             method: String,
             json: String,
             refs: Vec<String>,
             resolve: Function<'js>,
             reject: Function<'js>| {
                JsState::push(Op::Call {
                    target: parse_id(&target),
                    method,
                    payload: Payload::from_parts(json.into_bytes(), parse_ids(&refs)),
                    resolve: Persistent::save(&ctx, resolve),
                    reject: Persistent::save(&ctx, reject),
                });
            },
        )?,
    )?;
    rt.set(
        "observe",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, target: String, callback: Function<'js>| {
                JsState::push(Op::Observe {
                    target: parse_id(&target),
                    callback: Persistent::save(&ctx, callback),
                });
            },
        )?,
    )?;
    rt.set(
        "openView",
        Function::new(ctx.clone(), |surface: String| -> f64 {
            let key = JsState::with(|state| {
                state.next_view += 1;
                state.next_view
            });
            JsState::push(Op::OpenView {
                surface: parse_id(&surface),
                key,
            });
            key as f64
        })?,
    )?;
    rt.set(
        "render",
        Function::new(ctx.clone(), |key: f64, tree: String| {
            JsState::push(Op::Render {
                key: key as u32,
                tree,
            });
        })?,
    )?;
    rt.set(
        "respond",
        Function::new(
            ctx.clone(),
            |request: f64, json: String, refs: Vec<String>, is_error: bool| {
                let outcome = if is_error {
                    Err(serde_json::from_str::<String>(&json).unwrap_or(json))
                } else {
                    Ok(Payload::from_parts(json.into_bytes(), parse_ids(&refs)))
                };
                JsState::push(Op::Respond {
                    request: request as u64,
                    outcome,
                });
            },
        )?,
    )?;
    rt.set(
        "log",
        Function::new(ctx.clone(), |message: String| {
            eprintln!("[js] {message}");
        })?,
    )?;
    ctx.globals().set("__rt", rt)
}

fn prelude_fn<'js>(ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<Function<'js>> {
    let prelude: rquickjs::Object = ctx.globals().get("__prelude")?;
    prelude.get(name)
}

fn parse_id(id: &str) -> u64 {
    id.parse().unwrap_or_else(|_| {
        log::error!("js_runtime: malformed object id {id:?}");
        0
    })
}

fn parse_ids(ids: &[String]) -> Vec<u64> {
    ids.iter().map(|id| parse_id(id)).collect()
}

fn ref_strings(refs: &[u64]) -> Vec<String> {
    refs.iter().map(|id| id.to_string()).collect()
}

// ---------------------------------------------------------------------------------
// UI as data.
// ---------------------------------------------------------------------------------

/// A node of the script's UI tree, as `prelude.js` serializes it.
#[derive(Deserialize)]
struct Node {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    style: Style,
    #[serde(default)]
    children: Vec<Node>,
    text: Option<String>,
    on_click: Option<u32>,
}

/// The style vocabulary a script may use: a small, flat subset of GPUI's `Styled`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Style {
    flex: bool,
    flex_col: bool,
    items_center: bool,
    justify_center: bool,
    size_full: bool,
    w: Option<f32>,
    h: Option<f32>,
    p: Option<f32>,
    px: Option<f32>,
    py: Option<f32>,
    gap: Option<f32>,
    rounded: Option<f32>,
    border: Option<f32>,
    bg: Option<String>,
    border_color: Option<String>,
    text_color: Option<String>,
    text_size: Option<f32>,
}

struct JsView {
    tree: Option<Node>,
}

impl Render for JsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.tree {
            Some(tree) => render_node(tree),
            None => div().into_any_element(),
        }
    }
}

fn render_node(node: &Node) -> AnyElement {
    let style = &node.style;
    let mut element = div();
    if style.flex {
        element = element.flex();
    }
    if style.flex_col {
        element = element.flex_col();
    }
    if style.items_center {
        element = element.items_center();
    }
    if style.justify_center {
        element = element.justify_center();
    }
    if style.size_full {
        element = element.size_full();
    }
    if let Some(w) = style.w {
        element = element.w(px(w));
    }
    if let Some(h) = style.h {
        element = element.h(px(h));
    }
    if let Some(p) = style.p {
        element = element.p(px(p));
    }
    if let Some(p) = style.px {
        element = element.px(px(p));
    }
    if let Some(p) = style.py {
        element = element.py(px(p));
    }
    if let Some(gap) = style.gap {
        element = element.gap(px(gap));
    }
    if let Some(radius) = style.rounded {
        element = element.rounded(px(radius));
    }
    if let Some(width) = style.border {
        element = element.border(px(width));
    }
    if let Some(color) = style.bg.as_deref().and_then(parse_color) {
        element = element.bg(rgb(color));
    }
    if let Some(color) = style.border_color.as_deref().and_then(parse_color) {
        element = element.border_color(rgb(color));
    }
    if let Some(color) = style.text_color.as_deref().and_then(parse_color) {
        element = element.text_color(rgb(color));
    }
    if let Some(size) = style.text_size {
        element = element.text_size(px(size));
    }
    if let Some(handler) = node.on_click {
        element = element.on_mouse_down(MouseButton::Left, move |_, _, cx| {
            run_js(cx, |ctx| {
                let invoked = prelude_fn(ctx, "invokeHandler")
                    .and_then(|invoke: Function| invoke.call::<_, ()>((handler as f64,)));
                if let Err(error) = invoked.catch(ctx) {
                    log::error!("js_runtime: click handler failed: {error}");
                }
            });
        });
    }
    if node.kind == "text" {
        if let Some(text) = &node.text {
            element = element.child(text.clone());
        }
    } else {
        element = element.children(node.children.iter().map(render_node));
    }
    element.into_any_element()
}

/// `#rrggbb` (with or without the hash).
fn parse_color(text: &str) -> Option<u32> {
    u32::from_str_radix(text.trim_start_matches('#'), 16).ok()
}
