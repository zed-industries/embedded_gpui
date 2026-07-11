//! Demo host binary: opens a native GPUI window with two embedded plugin views driven by
//! the `example_plugin` guest component.
//!
//! The bootstrap is two root objects: this host installs its `DemoHost` root at the
//! reserved address 0 (`share_root`), and reaches the plugin through the plugin's
//! `DemoPlugin` root (`root()`). Every capability in either direction is a method call
//! from there; methods declared to return refs resolve directly with connected
//! `Remote`s.

use std::path::{Path, PathBuf};

use embedded_gpui::{
    PluginHost, PluginHostHandle as _, PluginOptions, PluginViewState, Remote, SharedRef, shared,
};
use embedded_gpui_util::Mirror;
use example_schema::{
    CommandApi, CommandApiCaller as _, Commands, CounterApi, DemoHost, DemoPlugin,
    DemoPluginCaller as _, Milestone, PaletteEntry, Text, WorkspaceApi,
};
use gpui::{
    App, Application, Bounds, Context, Entity, EventEmitter, MouseButton, Pixels, Task,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use std::collections::HashMap;

/// The home of the shared click counter: a plain host entity. The guest's views hold
/// remotes to it, call `increment`, and mirror `clicks`; native UI reads and mutates it
/// directly. `cx.notify` and `cx.emit(Milestone)` cross the boundary on their own.
struct Counter {
    clicks: u32,
}

impl EventEmitter<Milestone> for Counter {}

#[shared]
impl CounterApi for Counter {
    fn increment(&mut self, by: u32, cx: &mut Context<Self>) -> u32 {
        self.clicks += by;
        if self.clicks.is_multiple_of(5) {
            cx.emit(Milestone {
                clicks: self.clicks,
            });
        }
        cx.notify();
        self.clicks
    }

    fn clicks(&mut self, _cx: &mut Context<Self>) -> u32 {
        self.clicks
    }
}

/// A host service the PLUGIN drives: wasm buttons call `show_toast` / `set_accent`
/// through the schema's generated caller, and the native chrome reacts. One attribute
/// on the impl block is all the wiring there is.
struct Workspace {
    accent_hue: f32,
    last_toast: Option<String>,
}

/// The host's root object: the plugin's entire view of the host. Its methods mint (and
/// cache) refs to the entities the native UI also renders; handing the plugin a
/// different root — attenuated, audited, or fake — would change its whole world.
struct HostRoot {
    host: Entity<PluginHost>,
    counter: Entity<Counter>,
    workspace: Entity<Workspace>,
    counter_ref: Option<SharedRef<CounterApi>>,
    workspace_ref: Option<SharedRef<WorkspaceApi>>,
}

#[shared]
impl DemoHost for HostRoot {
    fn counter(&mut self, cx: &mut Context<Self>) -> SharedRef<CounterApi> {
        if let Some(reference) = self.counter_ref {
            return reference;
        }
        let reference = self.host.share(&self.counter, cx);
        self.counter_ref = Some(reference);
        reference
    }

    fn workspace(&mut self, cx: &mut Context<Self>) -> SharedRef<WorkspaceApi> {
        if let Some(reference) = self.workspace_ref {
            return reference;
        }
        let reference = self.host.share(&self.workspace, cx);
        self.workspace_ref = Some(reference);
        reference
    }
}

#[shared]
impl WorkspaceApi for Workspace {
    fn show_toast(&mut self, message: String, cx: &mut Context<Self>) -> String {
        self.last_toast = Some(message);
        cx.notify();
        "the host is showing your toast".to_string()
    }

    fn set_accent(&mut self, hue: f32, cx: &mut Context<Self>) -> String {
        self.accent_hue = hue.rem_euclid(1.0);
        cx.notify();
        format!("native accent set to hue {:.2}", self.accent_hue)
    }
}

fn main() {
    env_logger::init();

    let Some(wasm_path) = resolve_wasm_path() else {
        eprintln!("could not find or build example_plugin.wasm");
        std::process::exit(1);
    };

    let platform = gpui_platform::current_platform(false);
    let text_system = platform.text_system();

    Application::with_platform(platform).run(move |cx: &mut App| {
        // The whole embedding story: compile on the background, get a ready host.
        let plugin = PluginHost::load(wasm_path, PluginOptions::new(text_system), cx);
        cx.spawn(async move |cx| {
            let host = match plugin.await {
                Ok(host) => host,
                Err(error) => {
                    log::error!("embedded_gpui: failed to load plugin: {error:#}");
                    cx.update(|cx| cx.quit());
                    return;
                }
            };
            cx.update(move |cx| open_demo_window(host, cx));
        })
        .detach();
    });
}

fn open_demo_window(host: gpui::Entity<PluginHost>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(900.), px(700.)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |_window, cx| {
            let counter = cx.new(|_| Counter { clicks: 0 });
            let workspace = cx.new(|_| Workspace {
                accent_hue: 0.58,
                last_toast: None,
            });
            // The host's entire API surface, as one object: install the root at this
            // end's id 0. The plugin discovers the counter and the workspace by calling
            // its methods.
            let root = cx.new(|_| HostRoot {
                host: host.clone(),
                counter: counter.clone(),
                workspace: workspace.clone(),
                counter_ref: None,
                workspace_ref: None,
            });
            host.share_root(&root, cx);
            // Homed in the PLUGIN: the wasm input line's text and the command palette,
            // both reached through the plugin root's methods. Reads are calls, so
            // native rendering goes through local mirrors that refetch whenever the
            // plugin home notifies.
            let plugin = host.root::<DemoPlugin>(cx);
            let typed_text_receipt = plugin.typed_text(cx);
            let palette_receipt = plugin.palette(cx);
            // Views by name; each fills whatever slot the host lays out for it.
            let view0 = host.view("button", cx);
            let view1 = host.view("panel", cx);
            cx.new(|cx| {
                cx.observe(&counter, |_, _, cx| cx.notify()).detach();
                cx.observe(&workspace, |_, _, cx| cx.notify()).detach();
                // Discovery is asynchronous: the receipts resolve with connected
                // remotes, and the mirrors attach when they arrive.
                let discovery = cx.spawn(async move |this: gpui::WeakEntity<DemoView>, cx| {
                    let typed_text = typed_text_receipt.await;
                    let palette = palette_receipt.await;
                    cx.update(|cx| {
                        this.update(cx, |view: &mut DemoView, cx| {
                            match typed_text {
                                Ok(remote) => {
                                    let mirror = Mirror::new(remote, Text {}, cx);
                                    cx.observe(&mirror, |_, _, cx| cx.notify()).detach();
                                    view.typed_text = Some(mirror);
                                }
                                Err(error) => log::error!(
                                    "embedded_gpui: typed_text discovery failed: {error:#}"
                                ),
                            }
                            match palette {
                                Ok(remote) => {
                                    let mirror = Mirror::new(remote, Commands {}, cx);
                                    cx.observe(&mirror, |_, _, cx| cx.notify()).detach();
                                    view.palette = Some(mirror);
                                }
                                Err(error) => log::error!(
                                    "embedded_gpui: palette discovery failed: {error:#}"
                                ),
                            }
                            cx.notify();
                        })
                        .ok();
                    });
                });
                DemoView {
                    host,
                    _root: root,
                    counter,
                    workspace,
                    typed_text: None,
                    palette: None,
                    command_remotes: HashMap::new(),
                    command_status: None,
                    command_task: None,
                    _discovery: discovery,
                    view0,
                    view1,
                }
            })
        },
    );

    if let Err(error) = opened {
        log::error!("embedded_gpui: failed to open window: {error:#}");
        cx.quit();
        return;
    }

    cx.activate(true);
}

fn resolve_wasm_path() -> Option<PathBuf> {
    if let Some(argument) = std::env::args().nth(1) {
        return Some(PathBuf::from(argument));
    }

    let plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin");
    for profile in ["release", "debug"] {
        let candidate = plugin_dir
            .join("target/wasm32-wasip2")
            .join(profile)
            .join("example_plugin.wasm");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // First run: build the demo plugin ourselves so `cargo run` just works.
    eprintln!("building example_plugin for wasm32-wasip2 (first run only)...");
    // Blocking is fine here: this is a demo binary's startup path.
    #[allow(clippy::disallowed_methods)]
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&plugin_dir)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let built = plugin_dir.join("target/wasm32-wasip2/release/example_plugin.wasm");
    built.exists().then_some(built)
}

struct DemoView {
    host: Entity<PluginHost>,
    /// Keeps the root's ref caches alive; the registry holds it for the plugin anyway.
    _root: Entity<HostRoot>,
    counter: Entity<Counter>,
    workspace: Entity<Workspace>,
    /// `None` until the plugin root's `typed_text()` receipt resolves.
    typed_text: Option<Entity<Mirror<String>>>,
    /// `None` until the plugin root's `palette()` receipt resolves.
    palette: Option<Entity<Mirror<Vec<PaletteEntry>>>>,
    /// Remotes connected from palette refs, cached so repeated clicks reuse one
    /// projection (and so auto-release doesn't fire between clicks).
    command_remotes: HashMap<u64, Remote<CommandApi>>,
    command_status: Option<String>,
    command_task: Option<gpui::Task<()>>,
    _discovery: Task<()>,
    view0: Entity<PluginViewState>,
    view1: Entity<PluginViewState>,
}

impl DemoView {
    /// Invoke a palette command through its capability ref: connect (or reuse) a remote,
    /// call the schema-generated `invoke`, and surface the plugin's reply.
    fn run_command(&mut self, reference: SharedRef<CommandApi>, cx: &mut Context<Self>) {
        let command = self
            .command_remotes
            .entry(reference.entity_id())
            .or_insert_with(|| self.host.connect(reference, cx))
            .clone();
        let receipt = command.invoke(cx);
        self.command_task = Some(cx.spawn(async move |this, cx| {
            let status = match receipt.await {
                Ok(status) => status,
                Err(error) => format!("command failed: {error:#}"),
            };
            this.update(cx, |this, cx| {
                this.command_status = Some(status);
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for DemoView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let clicks = self.counter.read(cx).clicks;
        let commands = self
            .palette
            .as_ref()
            .and_then(|palette| palette.read(cx).latest().cloned())
            .unwrap_or_default();
        let typed = self
            .typed_text
            .as_ref()
            .and_then(|typed_text| typed_text.read(cx).latest().cloned())
            .unwrap_or_default();
        let counter = self.counter.clone();
        let accent = gpui::hsla(self.workspace.read(cx).accent_hue, 0.65, 0.6, 1.0);
        let toast = self.workspace.read(cx).last_toast.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .text_xl()
                            .text_color(accent)
                            .child("GPUI embedded in GPUI"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9aa3af))
                            .child(format!("shared counter (native view): {clicks}")),
                    )
                    .child(
                        div()
                            .id("native-increment")
                            .px_2()
                            .py_1()
                            .rounded(px(6.))
                            .bg(rgb(0x3a3f45))
                            .hover(|style| style.bg(rgb(0x4a5058)))
                            .text_sm()
                            .child("+5 from native")
                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                counter.update(cx, |counter, cx| {
                                    counter.clicks += 5;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9aa3af))
                            .child(format!("wasm says: {typed:?}")),
                    ),
            )
            .child(framed_slot(px(240.), px(100.), self.view0.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x9aa3af))
                                    .child("plugin commands (native buttons):"),
                            )
                            .children(commands.into_iter().enumerate().map(|(index, entry)| {
                                div()
                                    .id(("palette-command", index))
                                    .px_2()
                                    .py_1()
                                    .rounded(px(6.))
                                    .bg(rgb(0x3a3f45))
                                    .hover(|style| style.bg(rgb(0x4a5058)))
                                    .text_sm()
                                    .child(entry.label.clone())
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.run_command(entry.command, cx);
                                        }),
                                    )
                            })),
                    )
                    .when_some(self.command_status.clone(), |this, status| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(rgb(0x8ec07c))
                                .child(format!("plugin replied: {status}")),
                        )
                    }),
            )
            .child(framed_slot(px(480.), px(320.), self.view1.clone()))
            .when_some(toast, |this, message| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(accent)
                        .bg(rgb(0x2a2f36))
                        .text_sm()
                        .child(format!("🍞 from the plugin: {message}")),
                )
            })
    }
}

fn framed_slot(width: Pixels, height: Pixels, view: Entity<PluginViewState>) -> impl IntoElement {
    div()
        .w(width)
        .h(height)
        .border_1()
        .border_color(rgb(0x3c3c3c))
        .child(view)
}
