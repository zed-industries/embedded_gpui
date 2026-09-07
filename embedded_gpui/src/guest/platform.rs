use crate::clipboard::{ClipboardApi, ClipboardApiCaller as _};
use crate::dispatcher::PluginDispatcher;
use crate::guest::objects;
use crate::surface::{Cursor, Geometry, HostWindow, SurfaceApi};
use crate::text_system::PluginTextSystem;
use crate::window::{PluginWindow, PluginWindowState, serialize_scene};
use crate::wit;
use anyhow::{Result, anyhow};
use embedded_gpui::Remote;
use futures::channel::oneshot;
use gpui::{
    Action, AnyView, AnyWindowHandle, App, AppContext as _, AsyncApp, AttachedRoot,
    BackgroundExecutor, Bounds, ClipboardItem, CursorStyle, DummyKeyboardMapper, Empty,
    ForegroundExecutor, IntoElement, Keymap, Menu, MenuItem, PathPromptOptions, Pixels, Platform,
    PlatformDisplay, PlatformInput, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, Point, Render, Task, ThermalState, Window,
    WindowAppearance, WindowBounds, WindowOptions, WindowParams, point, px, size,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// A view a plugin opened on a host surface.
struct SurfaceView {
    surface: Remote<SurfaceApi>,
    view: AnyView,
    /// Distinguishes registrations: a surface reopened while its previous view's
    /// release is still pending must not lose the new view to that release.
    generation: u64,
    /// Where the host put the surface, once it has said: the guest window mirroring the
    /// surface's host window, the root the view is attached as, and the slot's origin.
    /// Until then the view is not drawn, so its first frame is at the slot's real size.
    placed: Option<Placement>,
}

struct Placement {
    window: u64,
    root: AttachedRoot,
    origin: Point<Pixels>,
    /// The overlay the view's own subtree last shipped, kept so it can be sent again
    /// unchanged when the window's unowned overlay joins or leaves it.
    owned_overlay: Option<wit::DisplayList>,
}

/// One host-driven event for a surface's view, applied by the pump.
pub enum SurfaceEvent {
    Resize {
        surface: u64,
        geometry: Geometry,
    },
    Input {
        surface: u64,
        input: PlatformInput,
    },
    /// The host's Tab traversal entered the surface: focus its first (or last) stop.
    FocusEntered {
        surface: u64,
        backward: bool,
    },
}

/// The root view of a guest window. Nothing is drawn at the window level; every visible
/// thing is a surface's root.
struct MirrorRoot;

impl Render for MirrorRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The GPUI [`Platform`] implementation for Wasm plugin guests. There is no real display:
/// each window mirrors one host window (size and scale factor), and every host surface in
/// that window is a root of it at the slot's real origin. Rendering, text shaping, and
/// scheduling are delegated across the boundary.
pub struct PluginPlatform {
    dispatcher: Arc<PluginDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<PluginTextSystem>,
    display: Rc<PluginDisplay>,
    /// Guest windows, keyed by the host window each mirrors.
    windows: RefCell<HashMap<u64, Rc<PluginWindowState>>>,
    /// The host window the next `open_window` mirrors; set right before it.
    pending_window: Cell<Option<HostWindow>>,
    /// Views with a surface, keyed by surface object id.
    views: RefCell<HashMap<u64, SurfaceView>>,
    /// The surface each shared view object (`ViewApi` id) draws on: how a query
    /// addressed to a view finds its window.
    view_objects: RefCell<HashMap<u64, u64>>,
    next_generation: Cell<u64>,
    /// Geometry and input the host delivered this turn. `ViewApi` handlers run inside
    /// the registry's `App` borrow, and GPUI's window callbacks re-enter the app, so the
    /// pump applies these once the borrow is released.
    pending_events: RefCell<Vec<SurfaceEvent>>,
    /// The surface that most recently received input: where cursor changes apply.
    last_input_surface: Cell<Option<u64>>,
    /// A cursor style GPUI set since the last pump; flushed to `last_input_surface`.
    pending_cursor: Cell<Option<Cursor>>,
    /// The clipboard object the plugin was given, if any, with the subscription that
    /// keeps `clipboard` current.
    clipboard_remote: RefCell<Option<(Remote<ClipboardApi>, gpui::Subscription)>>,
    /// The host clipboard's text as last heard; what GPUI's synchronous read returns.
    clipboard: RefCell<Option<ClipboardItem>>,
    /// Text GPUI wrote to the clipboard since the last pump; forwarded as a call.
    clipboard_write: RefCell<Option<String>>,
}

impl PluginPlatform {
    pub fn new() -> Self {
        let dispatcher = Arc::new(PluginDispatcher::new());
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        Self {
            dispatcher,
            background_executor,
            foreground_executor,
            text_system: Arc::new(PluginTextSystem::new()),
            display: Rc::new(PluginDisplay::new()),
            windows: RefCell::new(HashMap::new()),
            pending_window: Cell::new(None),
            views: RefCell::new(HashMap::new()),
            view_objects: RefCell::new(HashMap::new()),
            next_generation: Cell::new(0),
            pending_events: RefCell::new(Vec::new()),
            last_input_surface: Cell::new(None),
            pending_cursor: Cell::new(None),
            clipboard_remote: RefCell::new(None),
            clipboard: RefCell::new(None),
            clipboard_write: RefCell::new(None),
        }
    }

    pub fn set_clipboard(&self, clipboard: Remote<ClipboardApi>, subscription: gpui::Subscription) {
        *self.clipboard_remote.borrow_mut() = Some((clipboard, subscription));
    }

    pub fn set_clipboard_text(&self, text: Option<String>) {
        *self.clipboard.borrow_mut() = text.map(ClipboardItem::new_string);
    }

    /// Forward what GPUI wrote to the clipboard as a call on the clipboard object.
    pub fn flush_clipboard_writes(&self, cx: &mut App) {
        let Some(text) = self.clipboard_write.borrow_mut().take() else {
            return;
        };
        // The copy is what a read returns until the host says otherwise.
        *self.clipboard.borrow_mut() = Some(ClipboardItem::new_string(text.clone()));
        if let Some((clipboard, _)) = self.clipboard_remote.borrow().as_ref() {
            clipboard.write(text, cx);
        }
    }

    pub fn dispatcher(&self) -> &PluginDispatcher {
        &self.dispatcher
    }

    /// Give `surface` a view to draw. Replaces a previous view of the same surface.
    /// Returns the registration's generation, which identifies it to
    /// [`remove_view`](Self::remove_view).
    pub fn add_view(&self, surface: Remote<SurfaceApi>, view: AnyView, cx: &mut App) -> u64 {
        let surface_id = surface.reference().entity_id();
        let previous = self.views.borrow_mut().remove(&surface_id);
        if let Some(previous) = previous {
            self.unplace(previous.placed, cx);
        }
        let generation = self.next_generation.get();
        self.next_generation.set(generation + 1);
        self.views.borrow_mut().insert(
            surface_id,
            SurfaceView {
                surface,
                view,
                generation,
                placed: None,
            },
        );
        generation
    }

    /// Record that the shared view object `view` draws on `surface`.
    pub fn bind_view_object(&self, view: u64, surface: u64) {
        self.view_objects.borrow_mut().insert(view, surface);
    }

    pub fn unbind_view_object(&self, view: u64) {
        self.view_objects.borrow_mut().remove(&view);
    }

    /// Answer a synchronous text-input query addressed to the view object `view`: a
    /// key-down dispatched through its window (answering whether the guest consumed it),
    /// or a question for the window's focused text field. Runs between turns, outside
    /// any `App` borrow.
    pub fn input_query(
        &self,
        view: u64,
        query: wit::InputQuery,
        async_app: &mut AsyncApp,
    ) -> wit::InputAnswer {
        let Some(surface) = self.view_objects.borrow().get(&view).copied() else {
            return wit::InputAnswer::None;
        };
        let Some((window_id, origin)) = self
            .views
            .borrow()
            .get(&surface)
            .and_then(|view| view.placed.as_ref())
            .map(|placed| (placed.window, placed.origin))
        else {
            return wit::InputAnswer::None;
        };
        let Some(window) = self.window(window_id) else {
            return wit::InputAnswer::None;
        };
        match query {
            wit::InputQuery::KeyDown(key_down) => {
                self.last_input_surface.set(Some(surface));
                let key_down_keystroke = keystroke_from_wire(key_down.keystroke);
                let input = PlatformInput::KeyDown(gpui::KeyDownEvent {
                    keystroke: key_down_keystroke.clone(),
                    is_held: key_down.is_held,
                    prefer_character_input: false,
                });
                let handled = match window.dispatch_input(input) {
                    Some(result) => !result.propagate || result.default_prevented,
                    None => return wit::InputAnswer::None,
                };
                if handled {
                    return wit::InputAnswer::Handled(true);
                }
                // Tab traversal, when the view left the key alone: move within the
                // window's tab order, or, at its edge, let go of focus and hand the key
                // back so the host's traversal continues past the surface.
                let keystroke = &key_down_keystroke;
                let plain_tab = keystroke.key == "tab"
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform;
                if !plain_tab {
                    return wit::InputAnswer::Handled(false);
                }
                let backward = keystroke.modifiers.shift;
                let Some(handle) = window.handle() else {
                    return wit::InputAnswer::Handled(false);
                };
                async_app.update(|cx| {
                    handle
                        .update(cx, |_, window, cx| {
                            if window.focus_at_tab_edge(backward) {
                                window.blur(cx);
                                wit::InputAnswer::Handled(false)
                            } else {
                                if backward {
                                    window.focus_prev(cx);
                                } else {
                                    window.focus_next(cx);
                                }
                                wit::InputAnswer::Handled(true)
                            }
                        })
                        .unwrap_or(wit::InputAnswer::Handled(false))
                })
            }
            // Two answers need the window itself, not just the handler.
            wit::InputQuery::TextInputConfiguration | wit::InputQuery::AcceptsTextInput => {
                let Some(handle) = window.handle() else {
                    return wit::InputAnswer::None;
                };
                let accepts = matches!(query, wit::InputQuery::AcceptsTextInput);
                window
                    .with_input_handler(|handler| {
                        async_app.update(|cx| {
                            handle
                                .update(cx, |_, gpui_window, cx| {
                                    if accepts {
                                        wit::InputAnswer::Accepts(
                                            handler.accepts_text_input(gpui_window, cx),
                                        )
                                    } else {
                                        wit::InputAnswer::Configuration(configuration_to_wire(
                                            handler.text_input_configuration(gpui_window, cx),
                                        ))
                                    }
                                })
                                .ok()
                        })
                    })
                    .flatten()
                    .unwrap_or(wit::InputAnswer::None)
            }
            query => window
                .with_input_handler(|handler| match query {
                    wit::InputQuery::KeyDown(_)
                    | wit::InputQuery::TextInputConfiguration
                    | wit::InputQuery::AcceptsTextInput => wit::InputAnswer::None,
                    wit::InputQuery::SetSelectedTextRange(range) => {
                        handler.set_selected_text_range(range_from_wire(range));
                        wit::InputAnswer::Done
                    }
                    wit::InputQuery::TextLength => handler
                        .text_length_utf16()
                        .map_or(wit::InputAnswer::None, |length| {
                            wit::InputAnswer::Index(length as u32)
                        }),
                    wit::InputQuery::TextInputEditableRange => handler
                        .text_input_editable_range()
                        .map_or(wit::InputAnswer::None, |range| {
                            wit::InputAnswer::Range(wire_range(range))
                        }),
                    wit::InputQuery::SelectedTextRange(ignore_disabled_input) => handler
                        .selected_text_range(ignore_disabled_input)
                        .map_or(wit::InputAnswer::None, |selection| {
                            wit::InputAnswer::Selection(wit::Utf16Selection {
                                range: wire_range(selection.range),
                                reversed: selection.reversed,
                            })
                        }),
                    wit::InputQuery::MarkedTextRange => handler
                        .marked_text_range()
                        .map_or(wit::InputAnswer::None, |range| {
                            wit::InputAnswer::Range(wire_range(range))
                        }),
                    wit::InputQuery::TextForRange(range) => {
                        let mut adjusted = None;
                        handler
                            .text_for_range(range_from_wire(range), &mut adjusted)
                            .map_or(wit::InputAnswer::None, |text| {
                                wit::InputAnswer::Text(wit::TextForRange {
                                    text,
                                    adjusted: adjusted.map(wire_range),
                                })
                            })
                    }
                    wit::InputQuery::ReplaceTextInRange(replace) => {
                        handler.replace_text_in_range(
                            replace.range.map(range_from_wire),
                            &replace.text,
                        );
                        wit::InputAnswer::Done
                    }
                    wit::InputQuery::ReplaceAndMarkTextInRange(replace) => {
                        handler.replace_and_mark_text_in_range(
                            replace.range.map(range_from_wire),
                            &replace.text,
                            replace.new_selected_range.map(range_from_wire),
                        );
                        wit::InputAnswer::Done
                    }
                    wit::InputQuery::UnmarkText => {
                        handler.unmark_text();
                        wit::InputAnswer::Done
                    }
                    wit::InputQuery::BoundsForRange(range) => handler
                        .bounds_for_range(range_from_wire(range))
                        .map_or(wit::InputAnswer::None, |bounds| {
                            wit::InputAnswer::Bounds(wit::Bounds {
                                origin: wit::Point {
                                    x: f32::from(bounds.origin.x - origin.x),
                                    y: f32::from(bounds.origin.y - origin.y),
                                },
                                size: wit::Extent {
                                    width: f32::from(bounds.size.width),
                                    height: f32::from(bounds.size.height),
                                },
                            })
                        }),
                    wit::InputQuery::CharacterIndexForPoint(point) => handler
                        .character_index_for_point(gpui::point(
                            px(point.x) + origin.x,
                            px(point.y) + origin.y,
                        ))
                        .map_or(wit::InputAnswer::None, |index| {
                            wit::InputAnswer::Index(index as u32)
                        }),
                })
                .unwrap_or(wit::InputAnswer::None),
        }
    }

    /// Stop drawing on `surface`, if `generation` is still the registration drawing there.
    pub fn remove_view(&self, surface: u64, generation: u64, cx: &mut App) {
        let removed = {
            let mut views = self.views.borrow_mut();
            if views
                .get(&surface)
                .is_some_and(|view| view.generation == generation)
            {
                views.remove(&surface)
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            self.unplace(removed.placed, cx);
        }
    }

    /// Detach a view's root, and close its window if that was the last root in it.
    fn unplace(&self, placed: Option<Placement>, cx: &mut App) {
        let Some(placed) = placed else {
            return;
        };
        let Some(window) = self.window(placed.window) else {
            return;
        };
        if let Some(handle) = window.handle() {
            let root = placed.root;
            handle.update(cx, |_, window, _| root.detach(window)).ok();
        }
        let still_used = self.views.borrow().values().any(|view| {
            view.placed
                .as_ref()
                .is_some_and(|other| other.window == placed.window)
        });
        if !still_used {
            self.windows.borrow_mut().remove(&placed.window);
            if let Some(handle) = window.handle() {
                handle
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        }
    }

    /// The guest window mirroring host window `id`, if open.
    fn window(&self, id: u64) -> Option<Rc<PluginWindowState>> {
        let mut windows = self.windows.borrow_mut();
        if windows.get(&id).is_some_and(|window| window.is_closed()) {
            windows.remove(&id);
        }
        windows.get(&id).cloned()
    }

    /// The open guest windows.
    pub fn windows(&self) -> Vec<Rc<PluginWindowState>> {
        let mut windows = self.windows.borrow_mut();
        windows.retain(|_, window| !window.is_closed());
        windows.values().cloned().collect()
    }

    /// The guest window mirroring `host`, opened if this is the first surface in it.
    fn ensure_window(
        &self,
        host: HostWindow,
        async_app: &mut AsyncApp,
    ) -> Result<Rc<PluginWindowState>> {
        if let Some(window) = self.window(host.id) {
            return Ok(window);
        }
        self.pending_window.set(Some(host));
        let handle: AnyWindowHandle = async_app.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(host.width), px(host.height)),
                    })),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| MirrorRoot),
            )
            .map(Into::into)
        })?;
        self.pending_window.set(None);
        let window = self
            .window(host.id)
            .ok_or_else(|| anyhow!("open_window did not create the guest window"))?;
        window.set_handle(handle);
        Ok(window)
    }

    /// Queue a host-driven event for the next pump.
    pub fn push_event(&self, event: SurfaceEvent) {
        self.pending_events.borrow_mut().push(event);
    }

    /// Apply the queued events, in order. Called outside any `App` borrow: GPUI's window
    /// callbacks borrow the app themselves.
    pub fn flush_events(&self, async_app: &mut AsyncApp) {
        let events = std::mem::take(&mut *self.pending_events.borrow_mut());
        for event in events {
            match event {
                SurfaceEvent::Resize { surface, geometry } => {
                    if let Err(error) = self.place(surface, geometry, async_app) {
                        log::error!("embedded_gpui: cannot place surface {surface}: {error:#}");
                    }
                }
                SurfaceEvent::Input { surface, input } => self.dispatch_input(surface, input),
                SurfaceEvent::FocusEntered { surface, backward } => {
                    self.focus_entered(surface, backward, async_app)
                }
            }
        }
    }

    /// Put a surface's view where the host says it is: attach it as a root of the guest
    /// window mirroring the surface's host window (moving it between windows if the
    /// surface moved), at the slot's bounds.
    fn place(&self, surface: u64, geometry: Geometry, async_app: &mut AsyncApp) -> Result<()> {
        if geometry.width <= 0. || geometry.height <= 0. {
            return Ok(());
        }
        let Some((view, placed)) = self.views.borrow().get(&surface).map(|view| {
            (
                view.view.clone(),
                view.placed.as_ref().map(|placed| placed.window),
            )
        }) else {
            return Ok(());
        };
        let window = self.ensure_window(geometry.window, async_app)?;
        window.sync_host(geometry.window);
        let bounds = Bounds {
            origin: point(px(geometry.x), px(geometry.y)),
            size: size(px(geometry.width), px(geometry.height)),
        };
        if placed.is_some_and(|window_id| window_id == geometry.window.id) {
            // Same window: move the root.
            let mut views = self.views.borrow_mut();
            let Some(placed) = views
                .get_mut(&surface)
                .and_then(|view| view.placed.as_mut())
            else {
                return Ok(());
            };
            placed.origin = bounds.origin;
            if let Some(handle) = window.handle() {
                let root = &placed.root;
                async_app.update(|cx| {
                    handle
                        .update(cx, |_, window, _| root.set_bounds(window, bounds))
                        .ok();
                });
            }
            return Ok(());
        }
        // A new placement, or a move to another host window's mirror.
        let previous = self
            .views
            .borrow_mut()
            .get_mut(&surface)
            .and_then(|view| view.placed.take());
        async_app.update(|cx| self.unplace(previous, cx));
        let handle = window
            .handle()
            .ok_or_else(|| anyhow!("guest window has no handle"))?;
        let root = async_app
            .update(|cx| handle.update(cx, |_, window, _| window.attach_root(view, bounds)))?;
        if let Some(entry) = self.views.borrow_mut().get_mut(&surface) {
            entry.placed = Some(Placement {
                window: geometry.window.id,
                root,
                origin: bounds.origin,
                owned_overlay: None,
            });
        }
        Ok(())
    }

    /// Focus the first (or last) tab stop of the window the surface is in, from nothing:
    /// entering a region by keyboard starts at its edge, as it does in the host.
    fn focus_entered(&self, surface: u64, backward: bool, async_app: &mut AsyncApp) {
        let Some(handle) = self.window_of(surface).and_then(|window| window.handle()) else {
            return;
        };
        self.last_input_surface.set(Some(surface));
        async_app.update(|cx| {
            handle
                .update(cx, |_, window, cx| {
                    window.blur(cx);
                    if backward {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                })
                .ok();
        });
    }

    fn window_of(&self, surface: u64) -> Option<Rc<PluginWindowState>> {
        let window = self
            .views
            .borrow()
            .get(&surface)
            .and_then(|view| view.placed.as_ref())
            .map(|placed| placed.window)?;
        self.window(window)
    }

    /// Dispatch a host-forwarded input event (slot-relative) into the window the surface
    /// is placed in, at the slot's origin.
    fn dispatch_input(&self, surface: u64, mut input: PlatformInput) {
        let Some((window_id, origin)) = self
            .views
            .borrow()
            .get(&surface)
            .and_then(|view| view.placed.as_ref())
            .map(|placed| (placed.window, placed.origin))
        else {
            return;
        };
        let Some(window) = self.window(window_id) else {
            return;
        };
        self.last_input_surface.set(Some(surface));
        match &mut input {
            PlatformInput::MouseDown(event) => event.position += origin,
            PlatformInput::MouseUp(event) => event.position += origin,
            PlatformInput::MouseMove(event) => event.position += origin,
            PlatformInput::ScrollWheel(event) => event.position += origin,
            PlatformInput::MouseExited(event) => event.position += origin,
            _ => {}
        }
        window.dispatch_input(input);
    }

    /// Ship every root whose scene changed since it was last shipped, as its surface's
    /// display list, and every overlay that changed. Roots GPUI reused unchanged cost
    /// nothing here.
    ///
    /// A surface's overlay is what its own subtree deferred plus, for the surface the
    /// pointer is in, the window's unowned overlays (tooltips, the drag preview,
    /// prompts), since that is where those appear. The unowned part moves with the
    /// pointer: when it leaves one surface for another, both surfaces' overlays are sent
    /// again.
    pub fn ship_scenes(&self, cx: &mut App) {
        let mut views = self.views.borrow_mut();
        for window in self.windows() {
            let Some(handle) = window.handle() else {
                continue;
            };
            let scale_factor = window.scale_factor();
            let atlas = window.atlas().clone();
            let carrier = self.last_input_surface.get().filter(|surface| {
                views
                    .get(surface)
                    .and_then(|view| view.placed.as_ref())
                    .is_some_and(|placed| placed.window == window.host_id())
            });
            let previous_carrier = window.unowned_carrier.replace(carrier);
            handle
                .update(cx, |_, gpui_window, _| {
                    let unowned_changed = gpui_window.take_unowned_overlay_scene();
                    if let Some(scene) = unowned_changed {
                        let regions = gpui_window.unowned_overlay_hit_regions();
                        *window.unowned_overlay.borrow_mut() = Some((scene, regions));
                        window.mark_unowned_changed();
                    }
                    let unowned_changed = unowned_changed_flag(&window, carrier, previous_carrier);
                    for (surface, view) in views.iter_mut() {
                        let Some(placed) = view.placed.as_mut() else {
                            continue;
                        };
                        if placed.window != window.host_id() {
                            continue;
                        }
                        if let Some(scene) = placed.root.take_scene(gpui_window) {
                            let list = serialize_scene(&scene, scale_factor, placed.origin, &atlas);
                            objects::push_scene(*surface, list);
                        }
                        let owned_changed = match placed.root.take_overlay_scene(gpui_window) {
                            Some(scene) => {
                                let mut list =
                                    serialize_scene(&scene, scale_factor, placed.origin, &atlas);
                                list.hit_regions = wire_regions(
                                    placed.root.overlay_hit_regions(gpui_window),
                                    placed.origin,
                                );
                                placed.owned_overlay = Some(list);
                                true
                            }
                            None => false,
                        };
                        let is_carrier = carrier == Some(*surface);
                        let was_carrier = previous_carrier == Some(*surface);
                        let carrier_changed = is_carrier != was_carrier;
                        if !(owned_changed || carrier_changed || (is_carrier && unowned_changed)) {
                            continue;
                        }
                        let Some(owned) = placed.owned_overlay.as_mut() else {
                            continue;
                        };
                        let mut list = wit::DisplayList {
                            primitives: owned.primitives.clone(),
                            new_images: std::mem::take(&mut owned.new_images),
                            hit_regions: owned.hit_regions.clone(),
                        };
                        if is_carrier
                            && let Some((scene, regions)) = window.unowned_overlay.borrow().as_ref()
                        {
                            let mut unowned =
                                serialize_scene(scene, scale_factor, placed.origin, &atlas);
                            list.primitives.append(&mut unowned.primitives);
                            list.new_images.append(&mut unowned.new_images);
                            list.hit_regions
                                .extend(wire_regions(regions.clone(), placed.origin));
                        }
                        objects::push_overlay(*surface, list);
                    }
                })
                .ok();
        }
    }

    /// The cursor change GPUI requested since the last pump, and the surface it is for.
    pub fn take_pending_cursor(&self) -> Option<(Remote<SurfaceApi>, Cursor)> {
        let cursor = self.pending_cursor.take()?;
        let surface = self.last_input_surface.get()?;
        let remote = self.views.borrow().get(&surface)?.surface.clone();
        Some((remote, cursor))
    }
}

impl Platform for PluginPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        // The host drives the run loop through the `tick` export; launching completes
        // synchronously. The caller must hold `Application::app_cell` to keep the app alive.
        on_finish_launching();
    }

    fn quit(&self) {}

    fn restart(&self, _binary_path: Option<PathBuf>, _args: Vec<OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        None
    }

    fn open_window(
        &self,
        _handle: AnyWindowHandle,
        _params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let host = self.pending_window.take().ok_or_else(|| {
            anyhow!(
                "plugin windows mirror host windows; views are opened with embedded_gpui::open_view"
            )
        })?;
        let state = Rc::new(PluginWindowState::new(host, self.text_system.clone()));
        self.windows.borrow_mut().insert(host.id, state.clone());
        Ok(Box::new(PluginWindow::new(state, self.display.clone())))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn open_url(&self, _url: &str) {}

    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("url schemes are not supported in plugins")))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Err(anyhow!("path prompts are not supported in plugins")))
            .ok();
        receiver
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Err(anyhow!("path prompts are not supported in plugins")))
            .ok();
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn compositor_name(&self) -> &'static str {
        "GpuiPlugin"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow!("app_path is not available in plugins"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow!(
            "auxiliary executables are not available in plugins"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        self.pending_cursor.set(Some(Cursor::from_gpui(style)));
    }

    fn hide_cursor_until_mouse_moves(&self) {}

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.borrow().clone()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            *self.clipboard_write.borrow_mut() = Some(text);
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credentials are not available in plugins")))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credentials are not available in plugins")))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(PluginKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

struct PluginKeyboardLayout;

impl PlatformKeyboardLayout for PluginKeyboardLayout {
    fn id(&self) -> &str {
        "gpui-plugin"
    }

    fn name(&self) -> &str {
        "GPUI Plugin"
    }
}

#[derive(Debug)]
pub struct PluginDisplay {
    uuid: uuid::Uuid,
}

impl PluginDisplay {
    fn new() -> Self {
        Self {
            uuid: uuid::Uuid::from_u128(0x6770_7569_5f70_6c75_6769_6e00_0000_0001),
        }
    }
}

impl PlatformDisplay for PluginDisplay {
    fn id(&self) -> gpui::DisplayId {
        gpui::DisplayId::new(1)
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: size(px(8192.), px(8192.)),
        }
    }
}

fn keystroke_from_wire(keystroke: wit::Keystroke) -> gpui::Keystroke {
    gpui::Keystroke {
        modifiers: gpui::Modifiers {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            platform: keystroke.modifiers.platform,
            function: keystroke.modifiers.function,
        },
        key: keystroke.key,
        key_char: keystroke.key_char,
    }
}

fn wire_range(range: std::ops::Range<usize>) -> wit::TextRange {
    wit::TextRange {
        start: range.start as u32,
        end: range.end as u32,
    }
}

fn range_from_wire(range: wit::TextRange) -> std::ops::Range<usize> {
    range.start as usize..range.end as usize
}

fn configuration_to_wire(
    configuration: gpui::TextInputConfiguration,
) -> wit::TextInputConfiguration {
    wit::TextInputConfiguration {
        autocorrect: configuration.autocorrect,
        autocapitalize: match configuration.autocapitalize {
            gpui::Autocapitalize::None => wit::Autocapitalize::None,
            gpui::Autocapitalize::Words => wit::Autocapitalize::Words,
            gpui::Autocapitalize::Sentences => wit::Autocapitalize::Sentences,
            gpui::Autocapitalize::Characters => wit::Autocapitalize::Characters,
        },
        suggestions: configuration.suggestions,
        input_action: match configuration.input_action {
            gpui::TextInputAction::Unspecified => wit::TextInputAction::Unspecified,
            gpui::TextInputAction::Enter => wit::TextInputAction::Enter,
            gpui::TextInputAction::Done => wit::TextInputAction::Done,
            gpui::TextInputAction::Go => wit::TextInputAction::Go,
            gpui::TextInputAction::Next => wit::TextInputAction::Next,
            gpui::TextInputAction::Previous => wit::TextInputAction::Previous,
            gpui::TextInputAction::Search => wit::TextInputAction::Search,
            gpui::TextInputAction::Send => wit::TextInputAction::Send,
        },
    }
}

/// Whether the unowned overlay a carrier surface ships needs sending again: its scene
/// changed this frame, or the carrier moved.
fn unowned_changed_flag(
    window: &PluginWindowState,
    carrier: Option<u64>,
    previous_carrier: Option<u64>,
) -> bool {
    window.take_unowned_changed() || carrier != previous_carrier
}

fn wire_regions(regions: Vec<gpui::HitRegion>, origin: Point<Pixels>) -> Vec<wit::HitRegion> {
    regions
        .into_iter()
        .map(|region| wit::HitRegion {
            bounds: wit::Bounds {
                origin: wit::Point {
                    x: f32::from(region.bounds.origin.x - origin.x),
                    y: f32::from(region.bounds.origin.y - origin.y),
                },
                size: wit::Extent {
                    width: f32::from(region.bounds.size.width),
                    height: f32::from(region.bounds.size.height),
                },
            },
            block_mouse: region.behavior != gpui::HitboxBehavior::Normal,
        })
        .collect()
}
