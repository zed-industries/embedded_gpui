//! A guest window: the mirror of one host window. It has the host window's size and
//! scale factor, every host surface in that window is a root attached to it at the
//! slot's real origin (`Window::attach_root`), and after each frame each root's scene
//! is read back on its own (`Window::take_root_scene`) and shipped to its surface only
//! when it changed. The platform owns the table of surfaces and windows; this is one
//! window's state and its `PlatformWindow`.

use crate::platform::PluginDisplay;
use crate::surface::HostWindow;
use crate::text_system::{PluginAtlas, PluginTextSystem, TileContent};
use crate::wit;
use anyhow::Result;
use futures::channel::oneshot;
use gpui::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, ScaledPixels, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, point, px, size,
};
use raw_window_handle as rwh;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Once};

type RequestFrameCallback = Box<dyn FnMut(RequestFrameOptions)>;
type InputCallback = Box<dyn FnMut(PlatformInput) -> DispatchEventResult>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;
type ActiveCallback = Box<dyn FnMut(bool)>;
type AppearanceCallback = Box<dyn FnMut()>;

#[derive(Default)]
struct Callbacks {
    request_frame: Option<RequestFrameCallback>,
    input: Option<InputCallback>,
    resize: Option<ResizeCallback>,
    active: Option<ActiveCallback>,
    appearance: Option<AppearanceCallback>,
}

/// Shared state for one guest window: the platform's `PlatformWindow` and the surface
/// table reach it through one `Rc`.
pub struct PluginWindowState {
    host: Cell<HostWindow>,
    mouse_position: Cell<Point<Pixels>>,
    /// The modifiers the last input event carried: what GPUI reads back when an
    /// element asks which keys are held.
    modifiers: Cell<Modifiers>,
    /// Whether the pointer is over one of this window's surfaces.
    hovered: Cell<bool>,
    atlas: Arc<PluginAtlas>,
    callbacks: RefCell<Callbacks>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    handle: Cell<Option<AnyWindowHandle>>,
    /// GPUI dropped its `PlatformWindow`: nothing may be dispatched to this state again.
    closed: Cell<bool>,
}

impl PluginWindowState {
    pub fn new(host: HostWindow, text_system: Arc<PluginTextSystem>) -> Self {
        Self {
            host: Cell::new(host),
            mouse_position: Cell::new(Point::default()),
            modifiers: Cell::new(Modifiers::default()),
            hovered: Cell::new(false),
            atlas: Arc::new(PluginAtlas::new(text_system)),
            callbacks: RefCell::new(Callbacks::default()),
            input_handler: RefCell::new(None),
            handle: Cell::new(None),
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

    pub fn atlas(&self) -> &Arc<PluginAtlas> {
        &self.atlas
    }

    pub fn scale_factor(&self) -> f32 {
        self.host.get().scale_factor
    }

    /// The host window this one mirrors.
    pub fn host_id(&self) -> u64 {
        self.host.get().id
    }

    fn size(&self) -> Size<Pixels> {
        let host = self.host.get();
        size(px(host.width), px(host.height))
    }

    /// Follow the host window: a change of size or scale factor re-lays the window out,
    /// and active state and appearance are reported as GPUI expects from a platform.
    /// Called outside any `App` borrow, since GPUI's callbacks borrow the app.
    pub fn sync_host(&self, host: HostWindow) {
        let current = self.host.get();
        if current == host {
            return;
        }
        self.host.set(host);
        if current.width != host.width
            || current.height != host.height
            || current.scale_factor != host.scale_factor
        {
            let callback = self.callbacks.borrow_mut().resize.take();
            if let Some(mut callback) = callback {
                callback(self.size(), host.scale_factor);
                self.callbacks.borrow_mut().resize = Some(callback);
            }
        }
        if current.active != host.active {
            let callback = self.callbacks.borrow_mut().active.take();
            if let Some(mut callback) = callback {
                callback(host.active);
                self.callbacks.borrow_mut().active = Some(callback);
            }
        }
        if current.appearance != host.appearance {
            let callback = self.callbacks.borrow_mut().appearance.take();
            if let Some(mut callback) = callback {
                callback();
                self.callbacks.borrow_mut().appearance = Some(callback);
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

    /// Dispatch an input event, already in this window's coordinates, through GPUI's
    /// input pipeline. Text is not synthesized from unhandled keys here: the host's
    /// platform does that (or its IME does), and it lands in the focused field through
    /// [`with_input_handler`](Self::with_input_handler).
    pub fn dispatch_input(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        if self.closed.get() {
            return None;
        }
        match &input {
            PlatformInput::MouseDown(event) => {
                self.mouse_position.set(event.position);
                self.modifiers.set(event.modifiers);
                self.hovered.set(true);
            }
            PlatformInput::MouseUp(event) => {
                self.mouse_position.set(event.position);
                self.modifiers.set(event.modifiers);
                self.hovered.set(true);
            }
            PlatformInput::MouseMove(event) => {
                self.mouse_position.set(event.position);
                self.modifiers.set(event.modifiers);
                self.hovered.set(true);
            }
            PlatformInput::ScrollWheel(event) => {
                self.mouse_position.set(event.position);
                self.modifiers.set(event.modifiers);
                self.hovered.set(true);
            }
            PlatformInput::MouseExited(event) => {
                self.mouse_position.set(event.position);
                self.modifiers.set(event.modifiers);
                self.hovered.set(false);
            }
            PlatformInput::KeyDown(event) => self.modifiers.set(event.keystroke.modifiers),
            PlatformInput::KeyUp(event) => self.modifiers.set(event.keystroke.modifiers),
            PlatformInput::ModifiersChanged(event) => self.modifiers.set(event.modifiers),
            _ => {}
        }
        let callback = self.callbacks.borrow_mut().input.take();
        let mut callback = callback?;
        let result = callback(input);
        self.callbacks.borrow_mut().input = Some(callback);
        Some(result)
    }

    /// Run `f` with the focused element's input handler, if GPUI installed one (a
    /// focused text field). Called outside any `App` borrow: the handler borrows the app.
    pub fn with_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        if self.closed.get() {
            return None;
        }
        let mut handler = self.input_handler.take()?;
        let result = f(&mut handler);
        if self.input_handler.borrow().is_none() {
            self.input_handler.replace(Some(handler));
        }
        Some(result)
    }
}

/// The `PlatformWindow` handed to GPUI. GPUI owns this box; the platform reaches the
/// same state through the `Rc` it keeps.
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
            size: self.state.size(),
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
        let raw = rwh::WebWindowHandle::new(self.state.host.get().id as u32);
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
        self.state.size()
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // The window mirrors a host window; only the host resizes it.
    }

    fn scale_factor(&self) -> f32 {
        self.state.scale_factor()
    }

    fn appearance(&self) -> WindowAppearance {
        self.state.host.get().appearance.to_gpui()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.state.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.state.modifiers.get()
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
        self.state.host.get().active
    }

    fn is_hovered(&self) -> bool {
        self.state.hovered.get()
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

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.state.callbacks.borrow_mut().active = Some(callback);
    }

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.state.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.borrow_mut().appearance = Some(callback);
    }

    fn draw(&self, _scene: &Scene) {
        // The window's scene as a whole is never presented: after each frame the platform
        // reads every surface root's scene on its own (`PluginPlatform::ship_scenes`).
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
/// factor, minus the slot's origin in the window); glyph sprites are mapped back to the
/// symbolic parameters remembered by the atlas so the host can rasterize them itself.
pub fn serialize_scene(
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
        hit_regions: Vec::new(),
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
