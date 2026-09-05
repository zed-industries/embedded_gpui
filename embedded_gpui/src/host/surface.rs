//! The host-side [`Surface`]: the entity behind a `SurfaceApi` object. It caches the
//! guest's most recent display list and replays it every frame without calling into
//! the guest (DESIGN.md invariant 1), and drives the attached `ViewApi` with resize and
//! input as ordinary method calls.

use gpui::{
    App, Bounds, BoxShadow, ContentMask, Context, Corners, Edges, FocusHandle, IntoElement,
    KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, PlatformInput, Point, Render, ScrollWheelEvent, Size, UnderlineStyle, Window, canvas,
    div, point, prelude::*, px,
};

use crate::surface::{
    Cursor, Geometry, KeyEvent, MouseEvent, SurfaceApi, ViewApi, ViewApiCaller as _,
};
use crate::{PluginImages, Ref, Remote, bindings};

/// A place pixels go: one slot in the host's element tree, as a GPUI entity.
///
/// Create one, share it with a plugin host (`host.share(&surface, cx)` gives the
/// `Ref<SurfaceApi>` to pass through any plugin method), and place the entity in your
/// element tree like any view; it fills its slot. The guest attaches a view, which then
/// receives the slot's geometry and input.
///
/// A surface's lifetime belongs to its owner: sharing it does not keep it alive (see
/// [`Shared::keep_alive`](crate::Shared::keep_alive)). Drop the entity and the guest's
/// remote dangles, its view is released, and the window behind it closes.
pub struct Surface {
    view: Option<Remote<ViewApi>>,
    display_list: Option<(bindings::DisplayList, PluginImages)>,
    cursor: Option<gpui::CursorStyle>,
    geometry: Option<Geometry>,
    last_origin: Point<Pixels>,
    focus_handle: FocusHandle,
}

impl Surface {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            view: None,
            display_list: None,
            cursor: None,
            geometry: None,
            last_origin: Point::default(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// The view currently drawing here, if a guest has attached one.
    pub fn view(&self) -> Option<&Remote<ViewApi>> {
        self.view.as_ref()
    }

    /// Whether a display list has arrived since the last attach.
    pub fn has_scene(&self) -> bool {
        self.display_list.is_some()
    }

    pub(crate) fn set_scene(
        &mut self,
        list: bindings::DisplayList,
        images: PluginImages,
        cx: &mut Context<Self>,
    ) {
        self.display_list = Some((list, images));
        cx.notify();
    }

    /// Forward input to the attached view. Fire-and-forget: the guest's own dispatch
    /// does hit-testing and runs listeners (DESIGN.md invariant 7).
    fn forward_mouse(&self, input: PlatformInput, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        if let Some(event) = MouseEvent::from_gpui(&input, self.last_origin) {
            view.mouse(event, cx);
        }
    }

    fn forward_key(&self, input: PlatformInput, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        if let Some(event) = KeyEvent::from_gpui(&input) {
            view.key(event, cx);
        }
    }

    /// Record the slot's measured geometry and push it to the view if it changed.
    fn measured(&mut self, size: Size<Pixels>, scale_factor: f32, cx: &mut Context<Self>) {
        let geometry = Geometry {
            width: f32::from(size.width),
            height: f32::from(size.height),
            scale_factor,
        };
        if self.geometry == Some(geometry) {
            return;
        }
        self.geometry = Some(geometry);
        if let Some(view) = &self.view {
            view.resize(geometry, cx);
        }
    }
}

#[crate::shared(keep_alive = false)]
impl SurfaceApi for Surface {
    fn attach(&mut self, view: Ref<ViewApi>, cx: &mut Context<Self>) {
        let view = view.connect();
        // The view renders at the slot's real size from its first frame: the geometry
        // the slot was last measured at goes out before any input can.
        if let Some(geometry) = self.geometry {
            view.resize(geometry, cx);
        }
        self.view = Some(view);
        self.display_list = None;
        cx.notify();
    }

    fn set_cursor(&mut self, cursor: Cursor, cx: &mut Context<Self>) {
        self.cursor = Some(cursor.to_gpui());
        cx.notify();
    }
}

impl Render for Surface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prepaint_entity = cx.entity();
        let paint_entity = cx.entity();

        div()
            .size_full()
            .id(("embedded-surface", cx.entity_id()))
            .track_focus(&self.focus_handle)
            .when_some(self.cursor, |this, cursor| this.cursor(cursor))
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.forward_mouse(PlatformInput::MouseDown(event.clone()), cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.forward_key(PlatformInput::KeyDown(event.clone()), cx);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, cx| {
                this.forward_key(PlatformInput::KeyUp(event.clone()), cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.forward_mouse(PlatformInput::MouseUp(event.clone()), cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.forward_mouse(PlatformInput::MouseUp(event.clone()), cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.forward_mouse(PlatformInput::MouseUp(event.clone()), cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                this.forward_mouse(PlatformInput::MouseMove(event.clone()), cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.forward_mouse(PlatformInput::ScrollWheel(event.clone()), cx);
            }))
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                        let scale = window.scale_factor();
                        prepaint_entity.update(cx, |this, cx| {
                            this.last_origin = bounds.origin;
                            this.measured(bounds.size, scale, cx);
                        });
                        bounds
                    },
                    move |bounds: Bounds<Pixels>,
                          _: Bounds<Pixels>,
                          window: &mut Window,
                          cx: &mut App| {
                        let surface = paint_entity.read(cx);
                        if let Some((list, images)) = surface.display_list.as_ref() {
                            let images = images.borrow();
                            replay(list, bounds, &images, window);
                        }
                    },
                )
                .size_full(),
            )
    }
}

/// Replay a guest display list into the host window. Coordinates on the wire are logical
/// pixels relative to the view's slot; the host adds the slot origin and paints through the
/// public `Window::paint_*` APIs (DESIGN.md invariant 5). Primitives are grouped by ascending
/// `order` and each group is painted inside its own `paint_layer` so guest stacking is
/// preserved (invariant 6).
fn replay(
    list: &bindings::DisplayList,
    slot: Bounds<Pixels>,
    images: &std::collections::HashMap<u64, std::sync::Arc<gpui::RenderImage>>,
    window: &mut Window,
) {
    let mut indices: Vec<usize> = (0..list.primitives.len()).collect();
    indices.sort_by_key(|&index| list.primitives[index].order);

    let mut cursor = 0;
    while cursor < indices.len() {
        let order = list.primitives[indices[cursor]].order;
        let mut end = cursor + 1;
        while end < indices.len() && list.primitives[indices[end]].order == order {
            end += 1;
        }
        let layer = &indices[cursor..end];
        window.paint_layer(slot, |window| {
            for &index in layer {
                paint_primitive(&list.primitives[index].prim, slot, images, window);
            }
        });
        cursor = end;
    }
}

fn paint_primitive(
    primitive: &bindings::Primitive,
    slot: Bounds<Pixels>,
    images: &std::collections::HashMap<u64, std::sync::Arc<gpui::RenderImage>>,
    window: &mut Window,
) {
    match primitive {
        bindings::Primitive::Quad(quad) => {
            let bounds = to_bounds(&quad.bounds, slot.origin);
            let mask = to_bounds(&quad.content_mask, slot.origin).intersect(&slot);
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                window.paint_quad(PaintQuad {
                    bounds,
                    corner_radii: to_corners(&quad.corner_radii),
                    background: to_hsla(&quad.background).into(),
                    border_widths: to_edges(&quad.border_widths),
                    border_color: to_hsla(&quad.border_color),
                    border_style: to_border_style(quad.border_style),
                });
            });
        }
        bindings::Primitive::Shadow(shadow) => {
            let bounds = to_bounds(&shadow.bounds, slot.origin);
            let mask = to_bounds(&shadow.content_mask, slot.origin).intersect(&slot);
            let corner_radii = to_corners(&shadow.corner_radii);
            let box_shadow = BoxShadow {
                color: to_hsla(&shadow.color),
                offset: point(px(shadow.offset.x), px(shadow.offset.y)),
                blur_radius: px(shadow.blur_radius),
                spread_radius: px(shadow.spread_radius),
                inset: false,
            };
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                // HOST-INTEGRATION: gpui's public API is `paint_drop_shadows`, not the
                // `paint_shadows` named in the task; drop shadows are the wire's only kind.
                window.paint_drop_shadows(bounds, corner_radii, &[box_shadow]);
            });
        }
        bindings::Primitive::Underline(underline) => {
            let origin = to_point(&underline.origin, slot.origin);
            let mask = to_bounds(&underline.content_mask, slot.origin).intersect(&slot);
            let style = UnderlineStyle {
                color: Some(to_hsla(&underline.color)),
                thickness: px(underline.thickness),
                wavy: underline.wavy,
            };
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                window.paint_underline(origin, px(underline.width), &style);
            });
        }
        bindings::Primitive::Glyph(glyph) => {
            let origin = to_point(&glyph.origin, slot.origin);
            let mask = to_bounds(&glyph.content_mask, slot.origin).intersect(&slot);
            let font_id = gpui::FontId(glyph.font_id as usize);
            let glyph_id = gpui::GlyphId(glyph.glyph_id);
            let font_size = px(glyph.font_size);
            let color = to_hsla(&glyph.color);
            let is_emoji = glyph.is_emoji;
            let wire_glyph_id = glyph.glyph_id;
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                let result = if is_emoji {
                    window.paint_emoji(origin, font_id, glyph_id, font_size)
                } else {
                    window.paint_glyph(origin, font_id, glyph_id, font_size, color)
                };
                if let Err(error) = result {
                    log::warn!("embedded_gpui: failed to paint glyph {wire_glyph_id}: {error:#}");
                }
            });
        }
        bindings::Primitive::Path(path) => {
            if path.vertices.len() < 3 {
                return;
            }
            let mask = to_bounds(&path.content_mask, slot.origin).intersect(&slot);
            let color = to_hsla(&path.color);
            let mut rebuilt = gpui::Path::new(to_point(&path.vertices[0].xy, slot.origin));
            for triangle in path.vertices.chunks_exact(3) {
                rebuilt.push_triangle(
                    (
                        to_point(&triangle[0].xy, slot.origin),
                        to_point(&triangle[1].xy, slot.origin),
                        to_point(&triangle[2].xy, slot.origin),
                    ),
                    (
                        point(triangle[0].st.x, triangle[0].st.y),
                        point(triangle[1].st.x, triangle[1].st.y),
                        point(triangle[2].st.x, triangle[2].st.y),
                    ),
                );
            }
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                window.paint_path(rebuilt, color);
            });
        }
        bindings::Primitive::Image(image) => {
            static MISSING_IMAGE_WARNED: std::sync::Once = std::sync::Once::new();
            static OPACITY_WARNED: std::sync::Once = std::sync::Once::new();
            let Some(render_image) = images.get(&image.image_id) else {
                MISSING_IMAGE_WARNED.call_once(|| {
                    log::warn!(
                        "embedded_gpui: display list references image {} before its payload arrived",
                        image.image_id
                    );
                });
                return;
            };
            if image.opacity != 1.0 {
                OPACITY_WARNED.call_once(|| {
                    log::warn!(
                        "embedded_gpui: image opacity is not supported by the public paint API; \
                         painting fully opaque"
                    );
                });
            }
            let bounds = to_bounds(&image.bounds, slot.origin);
            let mask = to_bounds(&image.content_mask, slot.origin).intersect(&slot);
            let corner_radii = to_corners(&image.corner_radii);
            let render_image = render_image.clone();
            let grayscale = image.grayscale;
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                if let Err(error) =
                    window.paint_image(bounds, corner_radii, render_image, 0, grayscale)
                {
                    log::warn!("embedded_gpui: failed to paint image: {error:#}");
                }
            });
        }
    }
}

fn to_point(point: &bindings::Point, offset: Point<Pixels>) -> Point<Pixels> {
    gpui::point(px(point.x) + offset.x, px(point.y) + offset.y)
}

fn to_bounds(bounds: &bindings::Bounds, offset: Point<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: to_point(&bounds.origin, offset),
        size: gpui::size(px(bounds.size.width), px(bounds.size.height)),
    }
}

fn to_corners(corners: &bindings::Corners) -> Corners<Pixels> {
    Corners {
        top_left: px(corners.top_left),
        top_right: px(corners.top_right),
        bottom_right: px(corners.bottom_right),
        bottom_left: px(corners.bottom_left),
    }
}

fn to_edges(edges: &bindings::Edges) -> Edges<Pixels> {
    Edges {
        top: px(edges.top),
        right: px(edges.right),
        bottom: px(edges.bottom),
        left: px(edges.left),
    }
}

fn to_hsla(color: &bindings::Hsla) -> gpui::Hsla {
    gpui::hsla(color.h, color.s, color.l, color.a)
}

fn to_border_style(style: bindings::BorderStyle) -> gpui::BorderStyle {
    match style {
        bindings::BorderStyle::Solid => gpui::BorderStyle::Solid,
        bindings::BorderStyle::Dashed => gpui::BorderStyle::Dashed,
    }
}
