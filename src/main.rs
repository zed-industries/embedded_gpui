//! Demo host binary: opens a native GPUI window with two embedded plugin views driven by the
//! `example_plugin` guest component.

use std::path::{Path, PathBuf};

use gpui::{
    App, Application, Bounds, Context, Entity, MouseButton, Pixels, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
};
use gpui_embedded::{
    HandleShared, HostRemote, PluginHost, PluginInstance, PluginViewState, SharedEntitySource,
    SharedRef,
};
use gpui_embedded_shared::demo::{
    CommandApiCaller as _, CommandSpec, CounterSnapshot, CounterSpec, Increment, PaletteSpec,
    TextSpec, WorkspaceApi, WorkspaceSnapshot, WorkspaceSpec, register_workspace_api,
};
use std::collections::HashMap;

/// The home of the shared click counter: a plain host entity. The guest's views project it
/// and send `Increment` messages; native UI reads and mutates it directly.
struct Counter {
    clicks: u32,
}

impl SharedEntitySource<CounterSpec> for Counter {
    fn snapshot(&self, _cx: &App) -> CounterSnapshot {
        CounterSnapshot {
            clicks: self.clicks,
        }
    }
}

/// A host service the PLUGIN drives: wasm buttons call `show_toast` / `set_accent`
/// through the schema's generated caller, and the native chrome reacts. Implementing
/// the `WorkspaceApi` trait is all it takes; `register_workspace_api` wires it up.
struct Workspace {
    accent_hue: f32,
    last_toast: Option<String>,
}

impl SharedEntitySource<WorkspaceSpec> for Workspace {
    fn snapshot(&self, _cx: &App) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            accent_hue: self.accent_hue,
            last_toast: self.last_toast.clone(),
        }
    }
}

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

impl HandleShared<Increment> for Counter {
    fn handle(&mut self, message: Increment, cx: &mut Context<Self>) -> u32 {
        self.clicks += message.by;
        cx.notify();
        self.clicks
    }
}

fn main() {
    env_logger::init();

    let Some(wasm_path) = resolve_wasm_path() else {
        eprintln!("run build_plugin.sh first");
        std::process::exit(1);
    };

    let platform = gpui_platform::current_platform(false);
    let text_system = platform.text_system();

    Application::with_platform(platform).run(move |cx: &mut App| {
        let instance = match PluginInstance::new(&wasm_path, text_system) {
            Ok(instance) => instance,
            Err(error) => {
                log::error!("gpui_embedded: failed to load plugin: {error:#}");
                cx.quit();
                return;
            }
        };

        let bounds = Bounds::centered(None, size(px(900.), px(700.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let scale = window.scale_factor();
                let counter = cx.new(|_| Counter { clicks: 0 });
                let workspace = cx.new(|_| Workspace {
                    accent_hue: 0.58,
                    last_toast: None,
                });
                let host = cx.new(|_| PluginHost::new(instance));
                let (view0, view1, typed_text, palette) = host.update(cx, |host, cx| {
                    host.init(cx);
                    host.share(
                        &counter,
                        "clicks",
                        |methods| {
                            methods.on::<Increment>();
                        },
                        cx,
                    );
                    // Homed on the HOST, driven by the plugin: the workspace service.
                    host.share(&workspace, "workspace", register_workspace_api, cx);
                    // Homed in the GUEST: the wasm input line's text, projected natively.
                    let typed_text = host.remote::<TextSpec>("typed-text", cx);
                    // Also guest-homed: the command palette. Labels render as native
                    // buttons below; the refs they carry are the authority to invoke.
                    let palette = host.remote::<PaletteSpec>("palette", cx);
                    let view0 = host.create_view(0, size(px(240.), px(100.)), scale, cx);
                    let view1 = host.create_view(1, size(px(480.), px(320.)), scale, cx);
                    (view0, view1, typed_text, palette)
                });
                cx.new(|cx| {
                    cx.observe(&counter, |_, _, cx| cx.notify()).detach();
                    cx.observe(&workspace, |_, _, cx| cx.notify()).detach();
                    cx.observe(typed_text.replica(), |_, _, cx| cx.notify())
                        .detach();
                    cx.observe(palette.replica(), |_, _, cx| cx.notify())
                        .detach();
                    DemoView {
                        host,
                        counter,
                        workspace,
                        typed_text,
                        palette,
                        command_remotes: HashMap::new(),
                        command_status: None,
                        command_task: None,
                        view0,
                        view1,
                    }
                })
            },
        );

        if let Err(error) = opened {
            log::error!("gpui_embedded: failed to open window: {error:#}");
            cx.quit();
            return;
        }

        cx.activate(true);
    });
}

fn resolve_wasm_path() -> Option<PathBuf> {
    if let Some(argument) = std::env::args().nth(1) {
        return Some(PathBuf::from(argument));
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for profile in ["release", "debug"] {
        let candidate = manifest_dir
            .join("example_plugin/target/wasm32-wasip2")
            .join(profile)
            .join("example_plugin.wasm");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

struct DemoView {
    host: Entity<PluginHost>,
    counter: Entity<Counter>,
    workspace: Entity<Workspace>,
    typed_text: HostRemote<TextSpec>,
    palette: HostRemote<PaletteSpec>,
    /// Remotes materialized from palette refs, cached so repeated clicks reuse one
    /// projection (and so auto-release doesn't fire between clicks).
    command_remotes: HashMap<u64, HostRemote<CommandSpec>>,
    command_status: Option<String>,
    command_task: Option<gpui::Task<()>>,
    view0: Entity<PluginViewState>,
    view1: Entity<PluginViewState>,
}

impl DemoView {
    /// Invoke a palette command through its capability ref: materialize (or reuse) a
    /// remote, call the schema-generated `invoke`, and surface the plugin's reply.
    fn run_command(&mut self, reference: SharedRef<CommandSpec>, cx: &mut Context<Self>) {
        let command = self
            .command_remotes
            .entry(reference.entity_id())
            .or_insert_with(|| {
                self.host
                    .update(cx, |host, cx| host.remote_from_ref(reference, cx))
            })
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
            .replica()
            .read(cx)
            .state
            .as_ref()
            .map(|snapshot| snapshot.commands.clone())
            .unwrap_or_default();
        let typed = self
            .typed_text
            .replica()
            .read(cx)
            .state
            .as_ref()
            .map(|snapshot| snapshot.text.clone())
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
                    .child(div().text_xl().text_color(accent).child("GPUI embedded in GPUI"))
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
                    }))
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
