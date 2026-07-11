//! A demo GPUI plugin: two views (a button and a panel) sharing one guest App, rendered
//! by the `embedded_gpui` host. The panel exercises text, SVGs, images, paths, and
//! keyboard input. See `DESIGN.md`.
//!
//! The bootstrap is two root objects: this plugin installs its `DemoPlugin` root at
//! the reserved address 0 (`share_root`), and reaches the host through the host's
//! `DemoHost` root (`root()`). Every capability in either direction is a method call
//! from there; methods declared to return refs resolve directly with connected
//! `Remote`s.

use embedded_gpui::{
    Plugin, Receipt, Ref, Remote, register_plugin, root, share, share_root, shared,
};
use embedded_gpui_util::Mirror;
use example_schema::{
    Clicks, CommandApi, CounterApi, DemoHost, DemoHostCaller as _, DemoPlugin, Increment,
    Milestone, PaletteApi, PaletteEntry, TextApi, WorkspaceApi, WorkspaceApiCaller as _,
};
use gpui::{
    AnyView, App, AssetSource, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, KeyDownEvent, MouseButton, PathBuilder, Pixels, RenderImage, SharedString,
    Subscription, UTF16Selection, Window, canvas, div, hsla, img, point, prelude::*, px, rgb, svg,
};
use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

const STAR_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2l2.9 6.3 6.9.8-5.1 4.7 1.4 6.8L12 17.2 5.9 20.6l1.4-6.8L2.2 9.1l6.9-.8L12 2z"/></svg>"#;

struct PluginAssets;

impl AssetSource for PluginAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if path == "icons/star.svg" {
            Ok(Some(Cow::Borrowed(STAR_SVG.as_bytes())))
        } else {
            Ok(None)
        }
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec!["icons/star.svg".into()])
    }
}

struct ExamplePlugin {
    /// The host's root object: the single capability this plugin starts from.
    host: Remote<DemoHost>,
    /// This plugin's root object, installed at this end's id 0.
    root: Entity<PluginRoot>,
}

impl Plugin for ExamplePlugin {
    fn new(cx: &mut App) -> Self {
        let host = root::<DemoHost>();
        let plugin_root = cx.new(|_| PluginRoot::default());
        share_root(&plugin_root, cx);
        Self {
            host,
            root: plugin_root,
        }
    }

    fn create_view(&mut self, name: &str, _window: &mut Window, cx: &mut App) -> AnyView {
        match name {
            "button" => cx.new(|cx| ButtonView::new(self.host.clone(), cx)).into(),
            _ => {
                // The panel renders the same entities the root's methods publish: the
                // lazy factories converge on one input line and one wave, whichever
                // side asks first.
                let (input_line, wave) = self.root.update(cx, |root, cx| {
                    (root.ensure_input_line(cx), root.ensure_wave(cx))
                });
                cx.new(|cx| PanelView::new(self.host.clone(), input_line, wave, cx))
                    .into()
            }
        }
    }

    fn assets() -> Option<Box<dyn AssetSource>> {
        Some(Box::new(PluginAssets))
    }
}

register_plugin!(ExamplePlugin);

/// The plugin's root object: the host's entire view of this plugin. Each method is a
/// lazy factory — the entity is created on the first call (from either end) and cached,
/// so repeated calls return the same ref and `connect` on the host dedups to one
/// projection.
#[derive(Default)]
struct PluginRoot {
    input_line: Option<Entity<InputLine>>,
    input_line_ref: Option<Ref<TextApi>>,
    wave: Option<Entity<Wave>>,
    palette: Option<(Entity<Palette>, Ref<PaletteApi>)>,
    /// The root owns the command entities whose refs travel in the palette.
    commands: Vec<Entity<WaveCommand>>,
}

impl PluginRoot {
    fn ensure_input_line(&mut self, cx: &mut Context<Self>) -> Entity<InputLine> {
        if let Some(input_line) = &self.input_line {
            return input_line.clone();
        }
        let input_line = cx.new(InputLine::new);
        self.input_line = Some(input_line.clone());
        input_line
    }

    fn ensure_wave(&mut self, cx: &mut Context<Self>) -> Entity<Wave> {
        if let Some(wave) = &self.wave {
            return wave.clone();
        }
        let wave = cx.new(|_| Wave {
            speed: 0.15,
            hue: 0.85,
        });
        self.wave = Some(wave.clone());
        wave
    }
}

#[shared]
impl DemoPlugin for PluginRoot {
    fn typed_text(&mut self, cx: &mut Context<Self>) -> Ref<TextApi> {
        if let Some(reference) = self.input_line_ref {
            return reference;
        }
        let input_line = self.ensure_input_line(cx);
        let reference = share(&input_line, cx);
        self.input_line_ref = Some(reference);
        reference
    }

    fn palette(&mut self, cx: &mut Context<Self>) -> Ref<PaletteApi> {
        if let Some((_, reference)) = &self.palette {
            return *reference;
        }
        // The command palette: each command is an anonymously shared entity, and the
        // palette carries their refs. The host renders the labels as native buttons;
        // clicking one calls straight back into these closures, which mutate the wave
        // model the panel view observes.
        let wave = self.ensure_wave(cx);
        let command_table: [(&str, CommandAction); 4] = [
            ("Wave: faster", |wave, cx| {
                wave.speed = (wave.speed * 1.6).clamp(-1.2, 1.2);
                cx.notify();
                format!("wave speed is now {:.2}/tick", wave.speed)
            }),
            ("Wave: slower", |wave, cx| {
                wave.speed /= 1.6;
                cx.notify();
                format!("wave speed is now {:.2}/tick", wave.speed)
            }),
            ("Wave: reverse", |wave, cx| {
                wave.speed = -wave.speed;
                cx.notify();
                if wave.speed < 0.0 {
                    "the wave now runs right-to-left".to_string()
                } else {
                    "the wave now runs left-to-right".to_string()
                }
            }),
            ("Wave: recolor", |wave, cx| {
                wave.hue = (wave.hue + 0.13) % 1.0;
                cx.notify();
                format!("wave hue is now {:.2}", wave.hue)
            }),
        ];
        let mut commands = Vec::new();
        let mut entries = Vec::new();
        for (label, action) in command_table {
            let command = cx.new(|_| WaveCommand {
                wave: wave.clone(),
                action,
            });
            let reference = share(&command, cx);
            entries.push(PaletteEntry {
                label: label.to_string(),
                command: reference,
            });
            commands.push(command);
        }
        let palette = cx.new(|_| Palette { entries });
        let reference = share(&palette, cx);
        self.commands = commands;
        self.palette = Some((palette, reference));
        reference
    }
}

/// The wave's shared knobs: a plain model entity. Commands mutate it, and the panel
/// view — whenever one exists — observes it. State outlives views.
struct Wave {
    speed: f32,
    hue: f32,
}

struct ButtonView {
    /// The host counter, discovered through the host root's `counter()` method; `None`
    /// until that call's receipt resolves (real distributed behavior, not a hack).
    counter: Option<Remote<CounterApi>>,
    /// A local, observable cache of the host counter's value: snapshots as a library.
    clicks: Option<Entity<Mirror<u32>>>,
}

impl ButtonView {
    fn new(host: Remote<DemoHost>, cx: &mut Context<Self>) -> Self {
        let receipt = host.counter(cx);
        cx.spawn(async move |this, cx| {
            let counter = match receipt.await {
                Ok(counter) => counter,
                Err(error) => {
                    eprintln!("[example_plugin] counter discovery failed: {error:#}");
                    return;
                }
            };
            this.update(cx, |view, cx| {
                let clicks = Mirror::new(counter.clone(), Clicks {}, cx);
                cx.observe(&clicks, |_, _, cx| cx.notify()).detach();
                view.counter = Some(counter);
                view.clicks = Some(clicks);
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            counter: None,
            clicks: None,
        }
    }
}

impl Render for ButtonView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let click_count = self
            .clicks
            .as_ref()
            .and_then(|clicks| clicks.read(cx).latest().copied())
            .unwrap_or(0);
        let counter = self.counter.clone();
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .id("plugin-button")
            .rounded(px(10.))
            .bg(rgb(0x2d5a88))
            .hover(|style| style.bg(rgb(0x3f76ad)))
            .border_2()
            .border_color(rgb(0x69a2d6))
            .font_family("Helvetica")
            .text_color(gpui::white())
            .text_size(px(15.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, _, cx| {
                    let Some(counter) = counter.clone() else {
                        return;
                    };
                    // A call: resolves with the host handler's return value. The mirror
                    // refetches on the home's notify, so the label follows on its own.
                    let call = counter.call(Increment { by: 1 }, cx);
                    cx.spawn(async move |_, _| match call.await {
                        Ok(new_count) => {
                            eprintln!("[example_plugin] increment returned {new_count}")
                        }
                        Err(error) => eprintln!("[example_plugin] call failed: {error:#}"),
                    })
                    .detach();
                }),
            )
            .child(if self.counter.is_none() {
                "Connecting…".to_string()
            } else if click_count == 0 {
                "Click me!".to_string()
            } else {
                format!("Clicked {click_count}x")
            })
    }
}

struct PanelView {
    /// The host's workspace service; `None` until the host root's `workspace()` receipt
    /// resolves.
    workspace: Option<Remote<WorkspaceApi>>,
    clicks: Option<Entity<Mirror<u32>>>,
    /// Filled by the host counter's `Milestone` events: `cx.emit` crossing the boundary.
    last_milestone: Option<u32>,
    input_line: Entity<InputLine>,
    /// The wave knobs live in a model owned by the plugin root (palette commands mutate
    /// them); the panel only animates the phase and observes the rest.
    wave: Entity<Wave>,
    gradient: Arc<RenderImage>,
    wave_phase: f32,
    _animation: gpui::Task<()>,
    _milestones: Option<Subscription>,
}

/// A command the host can discover and invoke: an entity implementing the schema's
/// `CommandApi` interface, shared so its ref can travel in the palette. Each invocation
/// mutates the wave model and reports what it did.
type CommandAction = fn(&mut Wave, &mut Context<Wave>) -> String;

struct WaveCommand {
    wave: Entity<Wave>,
    action: CommandAction,
}

#[shared]
impl CommandApi for WaveCommand {
    fn invoke(&mut self, cx: &mut Context<Self>) -> String {
        let wave = self.wave.clone();
        wave.update(cx, |wave, cx| (self.action)(wave, cx))
    }
}

/// The registry the host renders natively: a list of labels plus the capability to run
/// each one. Homed in the plugin, reached through the root's `palette()` method.
struct Palette {
    entries: Vec<PaletteEntry>,
}

#[shared]
impl PaletteApi for Palette {
    fn commands(&mut self, _cx: &mut Context<Self>) -> Vec<PaletteEntry> {
        self.entries.clone()
    }
}

#[shared]
impl TextApi for InputLine {
    fn text(&mut self, _cx: &mut Context<Self>) -> String {
        self.text.clone()
    }
}

impl PanelView {
    fn new(
        host: Remote<DemoHost>,
        input_line: Entity<InputLine>,
        wave: Entity<Wave>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Discover the host's counter and workspace through its root: two receipts,
        // resolved in the background; the panel renders what it has in the meantime.
        let counter_receipt = host.counter(cx);
        let workspace_receipt = host.workspace(cx);
        cx.spawn(async move |this, cx| {
            match counter_receipt.await {
                Ok(counter) => {
                    this.update(cx, |panel, cx| {
                        let clicks = Mirror::new(counter.clone(), Clicks {}, cx);
                        cx.observe(&clicks, |_, _, cx| cx.notify()).detach();
                        // A typed event from the host home, exactly like a local
                        // `cx.subscribe`.
                        let weak_panel = cx.weak_entity();
                        let milestones = counter.subscribe::<Milestone>(cx, move |event, cx| {
                            let clicks = event.clicks;
                            weak_panel
                                .update(cx, |panel, cx| {
                                    panel.last_milestone = Some(clicks);
                                    cx.notify();
                                })
                                .ok();
                        });
                        panel.clicks = Some(clicks);
                        panel._milestones = Some(milestones);
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    eprintln!("[example_plugin] counter discovery failed: {error:#}")
                }
            }
            match workspace_receipt.await {
                Ok(workspace) => {
                    this.update(cx, |panel, cx| {
                        panel.workspace = Some(workspace);
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    eprintln!("[example_plugin] workspace discovery failed: {error:#}")
                }
            }
        })
        .detach();

        cx.observe(&wave, |_, _, cx| cx.notify()).detach();

        // Drives the wave at ~30fps through the guest's timer path: each await arms a
        // dispatcher timer, which asks the host for a wakeup via `request-tick`.
        let animation = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(33))
                    .await;
                let still_alive = this.update(cx, |this, cx| {
                    this.wave_phase += this.wave.read(cx).speed;
                    cx.notify();
                });
                if still_alive.is_err() {
                    break;
                }
            }
        });

        Self {
            workspace: None,
            clicks: None,
            last_milestone: None,
            input_line,
            wave,
            gradient: Arc::new(RenderImage::new(vec![image::Frame::new(gradient_bitmap(
                48, 48,
            ))])),
            wave_phase: 0.0,
            _animation: animation,
            _milestones: None,
        }
    }
}

/// A small generated bitmap, stored as premultiplied BGRA like every `RenderImage` frame.
fn gradient_bitmap(width: u32, height: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(width, height, |x, y| {
        let horizontal = x as f32 / width as f32;
        let vertical = y as f32 / height as f32;
        // Channel order is BGRA.
        image::Rgba([
            (200.0 * (1.0 - horizontal)) as u8,
            (160.0 * vertical) as u8,
            (240.0 * horizontal) as u8,
            255,
        ])
    })
}

impl Render for PanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let clicks = self
            .clicks
            .as_ref()
            .and_then(|clicks| clicks.read(cx).latest().copied())
            .unwrap_or(0);
        let bar_width = px(16. + (clicks as f32 * 14.) % 380.);
        let wave_hue = self.wave.read(cx).hue;
        let milestone = match self.last_milestone {
            Some(clicks) => format!("last milestone event: {clicks} clicks"),
            None => "no milestone events yet (every 5th click)".to_string(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(12.))
            .bg(rgb(0x1e2227))
            .border_2()
            .border_color(rgb(0x454b54))
            .font_family("Helvetica")
            .text_color(rgb(0xd8dee9))
            .child(
                div()
                    .text_size(px(20.))
                    .text_color(gpui::white())
                    .child("Wasm plugin panel"),
            )
            .child(div().text_size(px(14.)).child(format!(
                "The button view has been clicked {clicks} time{}.",
                if clicks == 1 { "" } else { "s" }
            )))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0x9aa3af))
                    .child(milestone),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        svg()
                            .path("icons/star.svg")
                            .w(px(22.))
                            .h(px(22.))
                            .text_color(hsla(0.13, 0.9, 0.6, 1.0)),
                    )
                    .child(
                        img(self.gradient.clone())
                            .w(px(48.))
                            .h(px(48.))
                            .rounded(px(8.)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0x9aa3af))
                            .child("an SVG asset and a generated image"),
                    ),
            )
            .child(self.input_line.clone())
            .when_some(self.workspace.clone(), |this, workspace| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x9aa3af))
                                .child("drive the host:"),
                        )
                        .child(panel_button("toast-host", "Toast the host", {
                            let workspace = workspace.clone();
                            move |cx| {
                                let receipt = workspace
                                    .show_toast("hello from inside the sandbox 👋".to_string(), cx);
                                log_reply(receipt, cx);
                            }
                        }))
                        .child(panel_button("tint-host", "Tint host to wave color", {
                            move |cx| {
                                let receipt = workspace.set_accent(wave_hue, cx);
                                log_reply(receipt, cx);
                            }
                        })),
                )
            })
            .child(wave_canvas(self.wave_phase, wave_hue))
            .child(
                div()
                    .h(px(10.))
                    .w(bar_width)
                    .rounded(px(5.))
                    .bg(hsla(0.55, 0.65, 0.55, 1.0)),
            )
    }
}

/// A tessellated path, drawn with GPUI's `PathBuilder` inside the guest and animated by a
/// guest-side timer.
fn wave_canvas(phase: f32, hue: f32) -> impl IntoElement {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds: Bounds<Pixels>, _prepaint, window: &mut Window, _cx: &mut App| {
            let mut builder = PathBuilder::stroke(px(2.));
            let steps = 60;
            for step in 0..=steps {
                let progress = step as f32 / steps as f32;
                let x = bounds.origin.x + bounds.size.width * progress;
                let y = bounds.origin.y
                    + bounds.size.height * 0.5
                    + px((progress * std::f32::consts::TAU * 2.0 + phase).sin() * 10.0);
                if step == 0 {
                    builder.move_to(point(x, y));
                } else {
                    builder.line_to(point(x, y));
                }
            }
            match builder.build() {
                Ok(path) => window.paint_path(path, hsla(hue, 0.6, 0.6, 1.0)),
                Err(error) => eprintln!("failed to build wave path: {error:#}"),
            }
        },
    )
    .w_full()
    .h(px(28.))
}

/// A wasm-side button that invokes a host capability when clicked.
fn panel_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(8.))
        .py(px(4.))
        .rounded(px(6.))
        .bg(rgb(0x3a3f45))
        .hover(|style| style.bg(rgb(0x4a5058)))
        .text_size(px(12.))
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, _, cx| on_click(cx))
}

/// Await a call receipt in the background and log the host's reply.
fn log_reply(receipt: Receipt<String>, cx: &mut App) {
    cx.spawn(async move |_| match receipt.await {
        Ok(reply) => eprintln!("[example_plugin] host replied: {reply}"),
        Err(error) => eprintln!("[example_plugin] host call failed: {error:#}"),
    })
    .detach();
}

/// A deliberately minimal editable line: enough of `EntityInputHandler` to receive text
/// through the input-handler pipeline (the same path a real editor uses), plus a backspace
/// key binding. Selections, marked text, and cursor movement are out of scope.
struct InputLine {
    focus_handle: FocusHandle,
    text: String,
}

impl InputLine {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            text: String::new(),
        }
    }
}

impl EntityInputHandler for InputLine {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.text.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text.push_str(text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text.push_str(new_text);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for InputLine {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let shown = if self.text.is_empty() && !focused {
            "click and type\u{2026}".to_string()
        } else if focused {
            format!("{}\u{258f}", self.text)
        } else {
            self.text.clone()
        };
        div()
            .id("input-line")
            .track_focus(&self.focus_handle)
            .relative()
            .w_full()
            .h(px(30.))
            .px(px(8.))
            .flex()
            .items_center()
            .rounded(px(6.))
            .bg(rgb(0x14171b))
            .border_1()
            .border_color(if focused {
                rgb(0x69a2d6)
            } else {
                rgb(0x454b54)
            })
            .text_size(px(13.))
            .text_color(if self.text.is_empty() && !focused {
                rgb(0x6f7883)
            } else {
                rgb(0xe6ebf2)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "backspace" {
                    this.text.pop();
                    cx.notify();
                }
            }))
            .child(shown)
            .child(
                // Registers the input handler each paint while focused, which is what
                // routes host-forwarded printable keys into `replace_text_in_range`.
                canvas(
                    |_bounds, _window, _cx| (),
                    move |bounds: Bounds<Pixels>, _prepaint, window: &mut Window, cx: &mut App| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}
