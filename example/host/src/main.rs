//! Demo host binary: opens a native GPUI window with two embedded plugin surfaces drawn
//! by the `example_plugin` guest component.
//!
//! The bootstrap is two root objects: this host installs its `DemoHost` root at the
//! reserved address 0 (`share_root`), and reaches the plugin through the plugin's
//! `DemoPlugin` root (`root()`). Every capability in either direction is a method call
//! from there; methods declared to return refs resolve directly with connected
//! `Remote`s.

use std::path::{Path, PathBuf};

use embedded_gpui::clipboard::ClipboardApi;
use embedded_gpui::{
    PluginHost, PluginHostHandle as _, PluginOptions, Ref, Remote, Surface, shared,
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
    counter_ref: Option<Ref<CounterApi>>,
    workspace_ref: Option<Ref<WorkspaceApi>>,
    clipboard_ref: Option<Ref<ClipboardApi>>,
}

#[shared]
impl DemoHost for HostRoot {
    fn counter(&mut self, cx: &mut Context<Self>) -> Ref<CounterApi> {
        if let Some(reference) = &self.counter_ref {
            return reference.clone();
        }
        let reference = self.host.share(&self.counter, cx);
        self.counter_ref = Some(reference.clone());
        reference
    }

    fn workspace(&mut self, cx: &mut Context<Self>) -> Ref<WorkspaceApi> {
        if let Some(reference) = &self.workspace_ref {
            return reference.clone();
        }
        let reference = self.host.share(&self.workspace, cx);
        self.workspace_ref = Some(reference.clone());
        reference
    }

    fn clipboard(&mut self, cx: &mut Context<Self>) -> Ref<ClipboardApi> {
        if let Some(reference) = &self.clipboard_ref {
            return reference.clone();
        }
        let clipboard = self.host.read(cx).clipboard();
        let reference = self.host.share(&clipboard, cx);
        self.clipboard_ref = Some(reference.clone());
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

    let Some(wasm_path) = resolve_wasm_path("plugin", "example_plugin") else {
        eprintln!("could not find or build example_plugin.wasm");
        std::process::exit(1);
    };
    let Some(js_runtime_path) = resolve_wasm_path("js_runtime", "js_runtime") else {
        eprintln!("could not find or build js_runtime.wasm");
        std::process::exit(1);
    };
    // The JavaScript plugin is a directory: the runtime component reads index.js from
    // it. This host mounts the directory and otherwise treats the plugin like any other.
    let js_plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../js_runtime/plugins");

    let platform = gpui_platform::current_platform(false);
    let text_system = platform.text_system();

    Application::with_platform(platform).run(move |cx: &mut App| {
        // The whole embedding story: compile on the background, get a ready host. Two
        // plugins here — one written in Rust, one in JavaScript — loaded the same way,
        // driven through the same schema, talking to the same host objects.
        let plugin = PluginHost::load(wasm_path, PluginOptions::new(text_system.clone()), cx);
        let js_runtime = PluginHost::load(
            js_runtime_path,
            PluginOptions::new(text_system).with_plugin_dir(js_plugin_dir),
            cx,
        );
        cx.spawn(async move |cx| {
            let (host, js_host) = match (plugin.await, js_runtime.await) {
                (Ok(host), Ok(js_host)) => (host, js_host),
                (Err(error), _) | (_, Err(error)) => {
                    log::error!("embedded_gpui: failed to load plugin: {error:#}");
                    cx.update(|cx| cx.quit());
                    return;
                }
            };
            cx.update(move |cx| open_demo_window(host, js_host, cx));
        })
        .detach();
    });
}

fn open_demo_window(host: Entity<PluginHost>, js_host: Entity<PluginHost>, cx: &mut App) {
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
                clipboard_ref: None,
            });
            host.share_root(&root, cx);
            // The JavaScript runtime is another plugin with its own registry: it gets
            // its own root over the same host entities.
            let js_root_entity = cx.new(|_| HostRoot {
                host: js_host.clone(),
                counter: counter.clone(),
                workspace: workspace.clone(),
                counter_ref: None,
                workspace_ref: None,
                clipboard_ref: None,
            });
            js_host.share_root(&js_root_entity, cx);
            let js_plugin = js_host.root::<DemoPlugin>(cx);
            let js_surface = cx.new(Surface::new);
            js_plugin.show_button(js_host.share(&js_surface, cx), cx);
            // Homed in the PLUGIN: the wasm input line's text and the command palette,
            // both reached through the plugin root's methods. Reads are calls, so
            // native rendering goes through local mirrors that refetch whenever the
            // plugin home notifies.
            let plugin = host.root::<DemoPlugin>(cx);
            let typed_text_receipt = plugin.typed_text(cx);
            let palette_receipt = plugin.palette(cx);
            // Surfaces are ordinary entities the host owns and places; the plugin is
            // handed a ref to each and draws whatever it likes there.
            let button_surface = cx.new(Surface::new);
            let panel_surface = cx.new(Surface::new);
            plugin.show_button(host.share(&button_surface, cx), cx);
            plugin.show_panel(host.share(&panel_surface, cx), cx);
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
                    _root: root,
                    _js_root: js_root_entity,
                    js_surface,
                    counter,
                    workspace,
                    typed_text: None,
                    palette: None,
                    command_remotes: HashMap::new(),
                    command_status: None,
                    command_task: None,
                    _discovery: discovery,
                    button_surface,
                    panel_surface,
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

/// Find (or build, on first run) a component crate under `example/`.
fn resolve_wasm_path(crate_dir: &str, crate_name: &str) -> Option<PathBuf> {
    let plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir);
    let file = format!("{crate_name}.wasm");
    for profile in ["release", "debug"] {
        let candidate = plugin_dir
            .join("target/wasm32-wasip2")
            .join(profile)
            .join(&file);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // First run: build the component ourselves so `cargo run` just works.
    eprintln!("building {crate_name} for wasm32-wasip2 (first run only)...");
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
    let built = plugin_dir.join("target/wasm32-wasip2/release").join(file);
    built.exists().then_some(built)
}

struct DemoView {
    /// Keeps the root's ref caches (and, through it, the plugin host) alive.
    _root: Entity<HostRoot>,
    _js_root: Entity<HostRoot>,
    js_surface: Entity<Surface>,
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
    button_surface: Entity<Surface>,
    panel_surface: Entity<Surface>,
}

impl DemoView {
    /// Invoke a palette command through its capability ref: connect (or reuse) a remote,
    /// call the schema-generated `invoke`, and surface the plugin's reply.
    fn run_command(&mut self, reference: Ref<CommandApi>, cx: &mut Context<Self>) {
        let command = self
            .command_remotes
            .entry(reference.entity_id())
            .or_insert_with(|| reference.connect())
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
            // Tab traversal for the whole demo window. A plugin surface is a tab stop
            // like any other, and a Tab the plugin leaves alone at the edge of its own
            // tab order arrives here, so focus moves on past the surface.
            .on_key_down(|event: &gpui::KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.key == "tab" && !keystroke.modifiers.platform {
                    if keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
            })
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(framed_slot(px(240.), px(100.), self.button_surface.clone()))
                    .child(framed_slot(px(240.), px(100.), self.js_surface.clone())),
            )
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
                                            this.run_command(entry.command.clone(), cx);
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
            .child(framed_slot(px(480.), px(320.), self.panel_surface.clone()))
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

fn framed_slot(width: Pixels, height: Pixels, surface: Entity<Surface>) -> impl IntoElement {
    div()
        .w(width)
        .h(height)
        .border_1()
        .border_color(rgb(0x3c3c3c))
        .child(surface)
}
