//! The guest's one window: a composition space in which every host surface is an
//! attached root (`Window::attach_root`). Surfaces are laid out in a grid of fixed cells
//! so that a surface's coordinates never depend on other surfaces (resizing one never
//! moves another, and hit-testing separates them by position alone), and each root's
//! scene is read back on its own (`Window::take_root_scene`) and shipped to its surface
//! only when it changed.

use crate::guest::objects;
use crate::platform::PluginDisplay;
use crate::surface::SurfaceApi;
use crate::text_system::{PluginAtlas, PluginTextSystem, TileContent};
use crate::wit;
use anyhow::{Result, anyhow};
use embedded_gpui::Remote;
use futures::channel::oneshot;
use gpui::{
    AnyView, AnyWindowHandle, App, AsyncApp, AttachedRootId, Bounds, Capslock, DispatchEventResult,
    GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    ScaledPixels, Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, point, px, size,
};
use raw_window_handle as rwh;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Once};

/// The side of one grid cell in logical pixels; the largest slot a surface can be.
pub const CELL_SIZE: f32 = 8192.;
const COLUMNS: usize = 32;
const ROWS: usize = 32;

/// The composition window's fixed size: the whole grid. Coordinates stay small enough
/// for `f32` to keep subpixel precision everywhere in it.
pub fn composition_size() -> Size<Pixels> {
    size(px(COLUMNS as f32 * CELL_SIZE), px(ROWS as f32 * CELL_SIZE))
}

fn cell_origin(cell: usize) -> Point<Pixels> {
    point(
        px((cell % COLUMNS) as f32 * CELL_SIZE),
        px((cell / COLUMNS) as f32 * CELL_SIZE),
    )
}

type RequestFrameCallback = Box<dyn FnMut(RequestFrameOptions)>;
type InputCallback = Box<dyn FnMut(PlatformInput) -> DispatchEventResult>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;

#[derive(Default)]
struct Callbacks {
    request_frame: Option<RequestFrameCallback>,
    input: Option<InputCallback>,
    resize: Option<ResizeCallback>,
}

/// One host surface's place in the composition window.
struct SurfaceRoot {
    surface: Remote<SurfaceApi>,
    view: AnyView,
    cell: usize,
    /// Distinguishes registrations: a surface reattached while its previous view's
    /// release is still pending must not lose the new view to that release.
    generation: u64,
    /// The GPUI root, once the host has sent the slot's geometry. Until then the view is
    /// not drawn, so its first frame is at the slot's real size.
    attached: Option<AttachedRootId>,
}

/// Shared state for the guest's composition window: the platform's `PlatformWindow`
/// and the exports reach it through one `Rc`.
pub struct PluginWindowState {
    scale_factor: Cell<f32>,
    mouse_position: Cell<Point<Pixels>>,
    atlas: Arc<PluginAtlas>,
    callbacks: RefCell<Callbacks>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    handle: Cell<Option<AnyWindowHandle>>,
    /// Surfaces with a view, keyed by surface object id.
    roots: RefCell<HashMap<u64, SurfaceRoot>>,
    free_cells: RefCell<Vec<usize>>,
    next_cell: Cell<usize>,
    next_generation: Cell<u64>,
    /// Shared with the platform: the surface that most recently received input.
    last_input_surface: Rc<Cell<Option<u64>>>,
    /// Geometry and input the host delivered this turn. `ViewApi` handlers run inside
    /// the registry's `App` borrow, and GPUI's window callbacks re-enter the app, so the
    /// pump applies these once the borrow is released.
    pending: RefCell<Vec<WindowEvent>>,
    /// GPUI dropped its `PlatformWindow`: nothing may be dispatched to this state again.
    closed: Cell<bool>,
}

/// One host-driven event, applied by the pump.
pub enum WindowEvent {
    Resize {
        surface: u64,
        size: Size<Pixels>,
        scale_factor: f32,
    },
    Input {
        surface: u64,
        input: PlatformInput,
    },
}

impl PluginWindowState {
    pub fn new(
        text_system: Arc<PluginTextSystem>,
        last_input_surface: Rc<Cell<Option<u64>>>,
    ) -> Self {
        Self {
            scale_factor: Cell::new(1.),
            mouse_position: Cell::new(Point::default()),
            atlas: Arc::new(PluginAtlas::new(text_system)),
            callbacks: RefCell::new(Callbacks::default()),
            input_handler: RefCell::new(None),
            handle: Cell::new(None),
            roots: RefCell::new(HashMap::new()),
            free_cells: RefCell::new(Vec::new()),
            next_cell: Cell::new(0),
            next_generation: Cell::new(0),
            last_input_surface,
            pending: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub fn set_handle(&self, handle: AnyWindowHandle) {
        self.handle.set(Some(handle));
    }

    pub fn handle(&self) -> Option<AnyWindowHandle> {
        self.handle.get()
    }

    /// Give `surface` a view to draw. Replaces a previous view of the same surface (a
    /// reattach). Returns the registration's generation, which identifies it to
    /// [`remove_surface`](Self::remove_surface).
    pub fn add_surface(
        &self,
        surface: Remote<SurfaceApi>,
        view: AnyView,
        cx: &mut App,
    ) -> Result<u64> {
        let surface_id = surface.reference().entity_id();
        let previous = self.roots.borrow_mut().remove(&surface_id);
        let cell = match previous {
            Some(previous) => {
                self.detach(previous.attached, cx);
                previous.cell
            }
            None => self.allocate_cell()?,
        };
        let generation = self.next_generation.get();
        self.next_generation.set(generation + 1);
        self.roots.borrow_mut().insert(
            surface_id,
            SurfaceRoot {
                surface,
                view,
                cell,
                generation,
                attached: None,
            },
        );
        Ok(generation)
    }

    /// Stop drawing on `surface`, if `generation` is still the registration drawing there.
    pub fn remove_surface(&self, surface: u64, generation: u64, cx: &mut App) {
        let removed = {
            let mut roots = self.roots.borrow_mut();
            if roots
                .get(&surface)
                .is_some_and(|root| root.generation == generation)
            {
                roots.remove(&surface)
            } else {
                None
            }
        };
        if let Some(root) = removed {
            self.detach(root.attached, cx);
            self.free_cells.borrow_mut().push(root.cell);
        }
    }

    /// The surface object behind a surface id, if a view is drawing on it.
    pub fn surface_remote(&self, surface: u64) -> Option<Remote<SurfaceApi>> {
        self.roots
            .borrow()
            .get(&surface)
            .map(|root| root.surface.clone())
    }

    fn allocate_cell(&self) -> Result<usize> {
        if let Some(cell) = self.free_cells.borrow_mut().pop() {
            return Ok(cell);
        }
        let cell = self.next_cell.get();
        if cell >= COLUMNS * ROWS {
            return Err(anyhow!(
                "embedded_gpui: at most {} surfaces can be open at once",
                COLUMNS * ROWS
            ));
        }
        self.next_cell.set(cell + 1);
        Ok(cell)
    }

    fn detach(&self, attached: Option<AttachedRootId>, cx: &mut App) {
        if let (Some(id), Some(handle)) = (attached, self.handle.get()) {
            handle
                .update(cx, |_, window, _| window.detach_root(id))
                .ok();
        }
    }

    /// Queue a host-driven event for the next pump.
    pub fn push_event(&self, event: WindowEvent) {
        self.pending.borrow_mut().push(event);
    }

    /// Apply the queued events, in order. Called outside any `App` borrow: GPUI's window
    /// callbacks borrow the app themselves.
    pub fn flush_events(&self, async_app: &mut AsyncApp) {
        let events = std::mem::take(&mut *self.pending.borrow_mut());
        if self.closed.get() {
            return;
        }
        for event in events {
            match event {
                WindowEvent::Resize {
                    surface,
                    size,
                    scale_factor,
                } => self.resized(surface, size, scale_factor, async_app),
                WindowEvent::Input { surface, input } => self.dispatch_input(surface, input),
            }
        }
    }

    /// Give GPUI a chance to redraw the window. GPUI's registered frame callback checks
    /// the window's dirty bit itself, so calling this on a clean window is cheap.
    ///
    /// The callback is temporarily moved out so that it can freely re-enter this window's
    /// other methods without hitting the `callbacks` RefCell.
    pub fn pump_frame(&self) {
        if self.closed.get() {
            return;
        }
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: false,
                force_render: false,
            });
            self.callbacks.borrow_mut().request_frame = Some(callback);
        }
    }

    /// Ship every root whose scene changed since it was last shipped, as that surface's
    /// display list. Roots GPUI reused unchanged cost nothing here.
    pub fn ship_scenes(&self, cx: &mut App) {
        let Some(handle) = self.handle.get() else {
            return;
        };
        if self.closed.get() {
            return;
        }
        let scale_factor = self.scale_factor.get();
        let roots = self.roots.borrow();
        handle
            .update(cx, |_, window, _| {
                for (surface_id, root) in roots.iter() {
                    let Some(attached) = root.attached else {
                        continue;
                    };
                    if let Some(scene) = window.take_root_scene(attached) {
                        let list = serialize_scene(
                            &scene,
                            scale_factor,
                            cell_origin(root.cell),
                            &self.atlas,
                        );
                        objects::push_scene(*surface_id, list);
                    }
                }
            })
            .ok();
    }

    /// Dispatch a host-forwarded input event through GPUI's input pipeline, translated
    /// from the surface's slot to its cell in the composition window.
    ///
    /// Unhandled printable key-downs fall through to the focused input handler, the same way
    /// GPUI's Linux backends synthesize text input from key events (there is no OS IME on
    /// this side of the wasm boundary).
    pub fn dispatch_input(&self, surface: u64, mut input: PlatformInput) {
        let Some(origin) = self
            .roots
            .borrow()
            .get(&surface)
            .map(|root| cell_origin(root.cell))
        else {
            return;
        };
        self.last_input_surface.set(Some(surface));
        match &mut input {
            PlatformInput::MouseDown(event) => {
                event.position += origin;
                self.mouse_position.set(event.position);
            }
            PlatformInput::MouseUp(event) => {
                event.position += origin;
                self.mouse_position.set(event.position);
            }
            PlatformInput::MouseMove(event) => {
                event.position += origin;
                self.mouse_position.set(event.position);
            }
            PlatformInput::ScrollWheel(event) => {
                event.position += origin;
                self.mouse_position.set(event.position);
            }
            _ => {}
        }
        let callback = self.callbacks.borrow_mut().input.take();
        let Some(mut callback) = callback else {
            return;
        };
        let result = callback(input.clone());
        self.callbacks.borrow_mut().input = Some(callback);

        if let PlatformInput::KeyDown(event) = input
            && result.propagate
            && !result.default_prevented
            && event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
            && let Some(key_char) = &event.keystroke.key_char
            && let Some(mut input_handler) = self.input_handler.take()
        {
            input_handler.replace_text_in_range(None, key_char);
            self.input_handler.replace(Some(input_handler));
        }
    }

    /// Apply a slot size or scale factor change coming from the host: the surface's root
    /// is attached at its cell (the first time) or moved to the new size.
    fn resized(
        &self,
        surface: u64,
        size: Size<Pixels>,
        scale_factor: f32,
        async_app: &mut AsyncApp,
    ) {
        static OVERSIZED_WARNED: Once = Once::new();
        if size.width <= Pixels::ZERO || size.height <= Pixels::ZERO {
            return;
        }
        let mut size = size;
        if size.width > px(CELL_SIZE) || size.height > px(CELL_SIZE) {
            warn_once(
                &OVERSIZED_WARNED,
                "embedded_gpui: a surface larger than 8192px is clipped to that size",
            );
            size.width = size.width.min(px(CELL_SIZE));
            size.height = size.height.min(px(CELL_SIZE));
        }
        if scale_factor != self.scale_factor.get() {
            // One scale factor per plugin: every surface of it is assumed to be on the
            // same display. GPUI re-lays the window out for the new factor.
            self.scale_factor.set(scale_factor);
            let callback = self.callbacks.borrow_mut().resize.take();
            if let Some(mut callback) = callback {
                callback(composition_size(), scale_factor);
                self.callbacks.borrow_mut().resize = Some(callback);
            }
        }
        let Some(handle) = self.handle.get() else {
            return;
        };
        let Some((view, cell, attached)) = self
            .roots
            .borrow()
            .get(&surface)
            .map(|root| (root.view.clone(), root.cell, root.attached))
        else {
            return;
        };
        let bounds = Bounds {
            origin: cell_origin(cell),
            size,
        };
        let attached = async_app.update(|cx| {
            handle
                .update(cx, |_, window, _| match attached {
                    Some(id) => {
                        window.set_root_bounds(id, bounds);
                        id
                    }
                    None => window.attach_root(view, bounds),
                })
                .ok()
        });
        if let Some(root) = self.roots.borrow_mut().get_mut(&surface) {
            root.attached = attached.or(root.attached);
        }
    }
}

/// The `PlatformWindow` handed to GPUI. GPUI owns this box; the platform and the exports
/// reach the same state through the `Rc` kept in `PluginPlatform`.
pub struct PluginWindow {
    state: Rc<PluginWindowState>,
    display: Rc<PluginDisplay>,
}

impl PluginWindow {
    pub fn new(state: Rc<PluginWindowState>, display: Rc<PluginDisplay>) -> Self {
        Self { state, display }
    }

    fn bounds_px(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: composition_size(),
        }
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        self.state.closed.set(true);
        *self.state.callbacks.borrow_mut() = Callbacks::default();
        self.state.input_handler.take();
    }
}

impl rwh::HasWindowHandle for PluginWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        // A synthetic handle: nothing consumes it, but the trait requires one.
        let raw = rwh::WebWindowHandle::new(1);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(rwh::RawWindowHandle::Web(raw)) })
    }
}

impl rwh::HasDisplayHandle for PluginWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let raw = rwh::WebDisplayHandle::new();
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(rwh::RawDisplayHandle::Web(raw)) })
    }
}

impl PlatformWindow for PluginWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds_px()
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds_px())
    }

    fn content_size(&self) -> Size<Pixels> {
        composition_size()
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // The composition window has a fixed size; the host owns every surface's geometry.
    }

    fn scale_factor(&self) -> f32 {
        self.state.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.state.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.state.input_handler.replace(Some(input_handler));
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.state.input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        true
    }

    fn is_hovered(&self) -> bool {
        true
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Transparent
    }

    fn set_title(&mut self, _title: &str) {}

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.state.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.state.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.state.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, _scene: &Scene) {
        // The composite scene is never presented as a whole: after each frame the pump
        // reads every surface root's scene on its own (`ship_scenes`).
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.state.atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}
}

fn warn_once(warned: &'static Once, message: &'static str) {
    warned.call_once(|| log::warn!("{message}"));
}

/// The mapping from a root's scene (scaled pixels, composition-window coordinates) to the
/// wire (logical pixels, relative to the surface's slot).
#[derive(Clone, Copy)]
struct Wire {
    inverse_scale: f32,
    origin: Point<f32>,
}

impl Wire {
    fn point(&self, value: Point<ScaledPixels>) -> wit::Point {
        wit::Point {
            x: value.x.0 * self.inverse_scale - self.origin.x,
            y: value.y.0 * self.inverse_scale - self.origin.y,
        }
    }

    fn bounds(&self, value: Bounds<ScaledPixels>) -> wit::Bounds {
        wit::Bounds {
            origin: self.point(value.origin),
            size: wit::Extent {
                width: value.size.width.0 * self.inverse_scale,
                height: value.size.height.0 * self.inverse_scale,
            },
        }
    }

    fn length(&self, value: ScaledPixels) -> f32 {
        value.0 * self.inverse_scale
    }
}

/// Convert one root's painted scene into the wire display list. Everything crossing the
/// boundary is in logical pixels relative to the surface's slot (divided by the scale
/// factor, minus the root's cell origin); glyph sprites are mapped back to the symbolic
/// parameters remembered by the atlas so the host can rasterize them itself.
fn serialize_scene(
    scene: &Scene,
    scale_factor: f32,
    origin: Point<Pixels>,
    atlas: &PluginAtlas,
) -> wit::DisplayList {
    static GRADIENT_WARNED: Once = Once::new();
    static SURFACE_WARNED: Once = Once::new();
    static SUBPIXEL_WARNED: Once = Once::new();
    static INSET_SHADOW_WARNED: Once = Once::new();
    static UNKNOWN_TILE_WARNED: Once = Once::new();
    static TRANSFORM_WARNED: Once = Once::new();

    let wire = Wire {
        inverse_scale: 1.0 / scale_factor,
        origin: point(f32::from(origin.x), f32::from(origin.y)),
    };
    let mut primitives = Vec::new();

    for quad in &scene.quads {
        let background = quad.background.as_solid().unwrap_or_else(|| {
            warn_once(
                &GRADIENT_WARNED,
                "embedded_gpui: gradient backgrounds are not supported; painting transparent",
            );
            gpui::transparent_black()
        });
        primitives.push(wit::PlacedPrimitive {
            order: quad.order,
            prim: wit::Primitive::Quad(wit::Quad {
                bounds: wire.bounds(quad.bounds),
                content_mask: wire.bounds(quad.content_mask.bounds),
                background: wire_hsla(background),
                border_color: wire_hsla(quad.border_color),
                corner_radii: wit::Corners {
                    top_left: wire.length(quad.corner_radii.top_left),
                    top_right: wire.length(quad.corner_radii.top_right),
                    bottom_right: wire.length(quad.corner_radii.bottom_right),
                    bottom_left: wire.length(quad.corner_radii.bottom_left),
                },
                border_widths: wit::Edges {
                    top: wire.length(quad.border_widths.top),
                    right: wire.length(quad.border_widths.right),
                    bottom: wire.length(quad.border_widths.bottom),
                    left: wire.length(quad.border_widths.left),
                },
                border_style: match quad.border_style {
                    gpui::BorderStyle::Solid => wit::BorderStyle::Solid,
                    gpui::BorderStyle::Dashed => wit::BorderStyle::Dashed,
                },
            }),
        });
    }

    for shadow in &scene.shadows {
        if shadow.inset != 0 {
            warn_once(
                &INSET_SHADOW_WARNED,
                "embedded_gpui: inset shadows are not supported; skipping",
            );
            continue;
        }
        let offset_x =
            (shadow.bounds.center().x.0 - shadow.element_bounds.center().x.0) * wire.inverse_scale;
        let offset_y =
            (shadow.bounds.center().y.0 - shadow.element_bounds.center().y.0) * wire.inverse_scale;
        let spread = ((shadow.bounds.size.width.0 - shadow.element_bounds.size.width.0) / 2.0
            - shadow.blur_radius.0)
            * wire.inverse_scale;
        primitives.push(wit::PlacedPrimitive {
            order: shadow.order,
            prim: wit::Primitive::Shadow(wit::Shadow {
                bounds: wire.bounds(shadow.element_bounds),
                content_mask: wire.bounds(shadow.content_mask.bounds),
                corner_radii: wit::Corners {
                    top_left: wire.length(shadow.element_corner_radii.top_left),
                    top_right: wire.length(shadow.element_corner_radii.top_right),
                    bottom_right: wire.length(shadow.element_corner_radii.bottom_right),
                    bottom_left: wire.length(shadow.element_corner_radii.bottom_left),
                },
                color: wire_hsla(shadow.color),
                blur_radius: wire.length(shadow.blur_radius),
                spread_radius: spread,
                offset: wit::Point {
                    x: offset_x,
                    y: offset_y,
                },
            }),
        });
    }

    for underline in &scene.underlines {
        primitives.push(wit::PlacedPrimitive {
            order: underline.order,
            prim: wit::Primitive::Underline(wit::Underline {
                origin: wire.point(underline.bounds.origin),
                width: wire.length(underline.bounds.size.width),
                content_mask: wire.bounds(underline.content_mask.bounds),
                color: wire_hsla(underline.color),
                thickness: wire.length(underline.thickness),
                wavy: underline.wavy == true.into(),
            }),
        });
    }

    for sprite in &scene.monochrome_sprites {
        if sprite.transformation != gpui::TransformationMatrix::unit() {
            warn_once(
                &TRANSFORM_WARNED,
                "embedded_gpui: sprite transformations are not supported; painting untransformed",
            );
        }
        match atlas.tile_content(sprite.tile.tile_id.0) {
            Some(TileContent::Glyph(params, raster_origin)) => {
                primitives.push(wit::PlacedPrimitive {
                    order: sprite.order,
                    prim: wit::Primitive::Glyph(wire_glyph(
                        &params,
                        raster_origin,
                        sprite.bounds,
                        sprite.content_mask.bounds,
                        sprite.color,
                        wire,
                    )),
                });
            }
            // A monochrome non-glyph sprite is a guest-rasterized SVG alpha mask: bake the
            // tint color in and ship it as an image.
            Some(TileContent::AlphaMask) => {
                if let Some(payload_id) = atlas.tinted_payload(sprite.tile.tile_id.0, sprite.color)
                {
                    primitives.push(wit::PlacedPrimitive {
                        order: sprite.order,
                        prim: wit::Primitive::Image(wit::Image {
                            image_id: payload_id,
                            bounds: wire.bounds(sprite.bounds),
                            content_mask: wire.bounds(sprite.content_mask.bounds),
                            corner_radii: wit::Corners {
                                top_left: 0.0,
                                top_right: 0.0,
                                bottom_right: 0.0,
                                bottom_left: 0.0,
                            },
                            grayscale: false,
                            opacity: 1.0,
                        }),
                    });
                }
            }
            _ => warn_once(
                &UNKNOWN_TILE_WARNED,
                "embedded_gpui: sprite refers to an unknown atlas tile; skipping",
            ),
        }
    }

    for sprite in &scene.polychrome_sprites {
        match atlas.tile_content(sprite.tile.tile_id.0) {
            Some(TileContent::Glyph(params, raster_origin)) => {
                primitives.push(wit::PlacedPrimitive {
                    order: sprite.order,
                    prim: wit::Primitive::Glyph(wire_glyph(
                        &params,
                        raster_origin,
                        sprite.bounds,
                        sprite.content_mask.bounds,
                        gpui::white(),
                        wire,
                    )),
                });
            }
            Some(TileContent::Bitmap) => {
                if let Some(payload_id) = atlas.bitmap_payload(sprite.tile.tile_id.0) {
                    primitives.push(wit::PlacedPrimitive {
                        order: sprite.order,
                        prim: wit::Primitive::Image(wit::Image {
                            image_id: payload_id,
                            bounds: wire.bounds(sprite.bounds),
                            content_mask: wire.bounds(sprite.content_mask.bounds),
                            corner_radii: wit::Corners {
                                top_left: wire.length(sprite.corner_radii.top_left),
                                top_right: wire.length(sprite.corner_radii.top_right),
                                bottom_right: wire.length(sprite.corner_radii.bottom_right),
                                bottom_left: wire.length(sprite.corner_radii.bottom_left),
                            },
                            grayscale: sprite.grayscale == true.into(),
                            opacity: sprite.opacity,
                        }),
                    });
                }
            }
            _ => warn_once(
                &UNKNOWN_TILE_WARNED,
                "embedded_gpui: sprite refers to an unknown atlas tile; skipping",
            ),
        }
    }

    for path in &scene.paths {
        let color = path.color.as_solid().unwrap_or_else(|| {
            warn_once(
                &GRADIENT_WARNED,
                "embedded_gpui: gradient backgrounds are not supported; painting transparent",
            );
            gpui::transparent_black()
        });
        primitives.push(wit::PlacedPrimitive {
            order: path.order,
            prim: wit::Primitive::Path(wit::Path {
                content_mask: wire.bounds(path.content_mask.bounds),
                color: wire_hsla(color),
                vertices: path
                    .vertices
                    .iter()
                    .map(|vertex| wit::PathVertex {
                        xy: wire.point(vertex.xy_position),
                        st: wit::Point {
                            x: vertex.st_position.x,
                            y: vertex.st_position.y,
                        },
                    })
                    .collect(),
            }),
        });
    }

    if !scene.surfaces.is_empty() {
        warn_once(
            &SURFACE_WARNED,
            "embedded_gpui: surface primitives are not supported; skipping",
        );
    }
    if !scene.subpixel_sprites.is_empty() {
        warn_once(
            &SUBPIXEL_WARNED,
            "embedded_gpui: subpixel sprites are not supported; skipping",
        );
    }

    wit::DisplayList {
        primitives,
        new_images: atlas.take_pending_payloads(),
    }
}

/// Reconstruct the symbolic glyph for a fabricated atlas tile. The baseline origin is the
/// sprite origin minus the raster-bounds offset that `Window::paint_glyph` added, plus the
/// subpixel variant's fractional offset, so the host re-derives the same variant and the
/// glyph lands where native rendering would put it.
fn wire_glyph(
    params: &gpui::RenderGlyphParams,
    raster_origin: gpui::Point<gpui::DevicePixels>,
    bounds: Bounds<ScaledPixels>,
    content_mask: Bounds<ScaledPixels>,
    color: gpui::Hsla,
    wire: Wire,
) -> wit::Glyph {
    let baseline = wire.point(point(
        ScaledPixels(
            bounds.origin.x.0 - raster_origin.x.0 as f32
                + params.subpixel_variant.x as f32 / gpui::SUBPIXEL_VARIANTS_X as f32,
        ),
        ScaledPixels(
            bounds.origin.y.0 - raster_origin.y.0 as f32
                + params.subpixel_variant.y as f32 / gpui::SUBPIXEL_VARIANTS_Y as f32,
        ),
    ));
    wit::Glyph {
        font_id: params.font_id.0 as u32,
        glyph_id: params.glyph_id.0,
        origin: baseline,
        font_size: f32::from(params.font_size),
        color: wire_hsla(color),
        content_mask: wire.bounds(content_mask),
        is_emoji: params.is_emoji,
    }
}

fn wire_hsla(color: gpui::Hsla) -> wit::Hsla {
    wit::Hsla {
        h: color.h,
        s: color.s,
        l: color.l,
        a: color.a,
    }
}
