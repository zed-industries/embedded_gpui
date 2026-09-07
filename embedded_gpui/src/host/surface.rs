//! The host-side [`Surface`]: the entity behind a `SurfaceApi` object. It caches the
//! guest's most recent display list and replays it every frame without calling into
//! the guest (DESIGN.md invariant 1), and drives the attached `ViewApi` with resize and
//! input as ordinary method calls.

use gpui::{
    App, Bounds, BoxShadow, ContentMask, Context, Corners, Edges, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, PlatformInput, Point, Render,
    ScrollWheelEvent, UnderlineStyle, Window, canvas, deferred, div, point, prelude::*, px, size,
};

use crate::surface::{
    Appearance, Cursor, Geometry, HostWindow, KeyEvent, MouseEvent, SurfaceApi, ViewApi,
    ViewApiCaller as _,
};

/// Overlays are deferred above everything the host defers itself.
const OVERLAY_PRIORITY: usize = 1 << 20;
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
    /// What the view drew outside its slot (popovers, tooltips, drag previews), painted
    /// above the whole host window, with the slot-relative bounds it covers.
    overlay: Option<(bindings::DisplayList, PluginImages, Bounds<Pixels>)>,
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
            overlay: None,
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

    pub(crate) fn set_overlay(
        &mut self,
        list: bindings::DisplayList,
        images: PluginImages,
        cx: &mut Context<Self>,
    ) {
        self.overlay = list_bounds(&list).map(|bounds| (list, images, bounds));
        cx.notify();
    }

    /// Mouse listeners forwarding to the view, for the slot and for its overlay alike.
    /// Positions are made slot-relative; the guest puts them back into window
    /// coordinates, so an overlay outside the slot works the same way.
    fn wire_mouse<E: StatefulInteractiveElement>(&self, element: E, cx: &mut Context<Self>) -> E {
        element
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.forward_mouse(PlatformInput::MouseDown(event.clone()), cx);
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
            // Leaving the element (or the window while over it): the guest clears hover
            // and dismisses tooltips, as it would for a pointer leaving a real window.
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                if !*hovered {
                    this.forward_mouse(
                        PlatformInput::MouseExited(MouseExitEvent {
                            position: window.mouse_position(),
                            pressed_button: None,
                            modifiers: window.modifiers(),
                        }),
                        cx,
                    );
                }
            }))
            .on_mouse_exit(cx.listener(|this, event: &MouseExitEvent, _window, cx| {
                this.forward_mouse(PlatformInput::MouseExited(event.clone()), cx);
            }))
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

    /// Record the slot's measured geometry — its bounds and the window it is in — and
    /// push it to the view if any of it changed.
    fn measured(&mut self, bounds: Bounds<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let geometry = Geometry {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            window: HostWindow {
                id: window.window_handle().window_id().as_u64(),
                width: f32::from(viewport.width),
                height: f32::from(viewport.height),
                scale_factor: window.scale_factor(),
                active: window.is_window_active(),
                appearance: Appearance::from_gpui(window.appearance()),
            },
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
        let overlay_entity = cx.entity();
        let overlay_bounds = self.overlay.as_ref().map(|(_, _, bounds)| *bounds);

        let slot = div()
            .size_full()
            .id(("embedded-surface", cx.entity_id()))
            .track_focus(&self.focus_handle)
            .when_some(self.cursor, |this, cursor| this.cursor(cursor))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.forward_key(PlatformInput::KeyDown(event.clone()), cx);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, cx| {
                this.forward_key(PlatformInput::KeyUp(event.clone()), cx);
            }))
            .on_modifiers_changed(cx.listener(
                |this, event: &ModifiersChangedEvent, _window, cx| {
                    this.forward_key(PlatformInput::ModifiersChanged(event.clone()), cx);
                },
            ));
        let slot = self.wire_mouse(slot, cx);

        let overlay = overlay_bounds.map(|bounds| {
            let overlay = div()
                .id(("embedded-overlay", cx.entity_id()))
                .absolute()
                .left(bounds.origin.x)
                .top(bounds.origin.y)
                .w(bounds.size.width)
                .h(bounds.size.height)
                .occlude()
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                            let surface = overlay_entity.read(cx);
                            if let Some((list, images, _)) = surface.overlay.as_ref() {
                                let images = images.borrow();
                                let clip = Bounds {
                                    origin: Point::default(),
                                    size: window.viewport_size(),
                                };
                                replay(list, surface.last_origin, clip, &images, window);
                            }
                        },
                    )
                    .size_full(),
                );
            deferred(self.wire_mouse(overlay, cx)).with_priority(OVERLAY_PRIORITY)
        });

        slot.child(
            canvas(
                move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                    prepaint_entity.update(cx, |this, cx| {
                        this.last_origin = bounds.origin;
                        this.measured(bounds, window, cx);
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
                        replay(list, bounds.origin, bounds, &images, window);
                    }
                },
            )
            .size_full(),
        )
        .children(overlay)
    }
}

/// The slot-relative bounds an overlay display list covers: where the host puts the
/// hitbox that routes input over it to the view.
fn list_bounds(list: &bindings::DisplayList) -> Option<Bounds<Pixels>> {
    let mut union: Option<Bounds<Pixels>> = None;
    let mut include = |bounds: Bounds<Pixels>| {
        union = Some(match union {
            Some(union) => union.union(&bounds),
            None => bounds,
        });
    };
    for placed in &list.primitives {
        match &placed.prim {
            bindings::Primitive::Quad(quad) => include(to_bounds(&quad.bounds, Point::default())),
            bindings::Primitive::Shadow(shadow) => {
                include(to_bounds(&shadow.bounds, Point::default()))
            }
            bindings::Primitive::Image(image) => {
                include(to_bounds(&image.bounds, Point::default()))
            }
            bindings::Primitive::Underline(underline) => include(Bounds {
                origin: to_point(&underline.origin, Point::default()),
                size: size(px(underline.width), px(underline.thickness)),
            }),
            bindings::Primitive::Glyph(glyph) => include(Bounds {
                origin: point(px(glyph.origin.x), px(glyph.origin.y - glyph.font_size)),
                size: size(px(glyph.font_size), px(glyph.font_size * 1.5)),
            }),
            bindings::Primitive::Path(path) => {
                for vertex in &path.vertices {
                    include(Bounds {
                        origin: to_point(&vertex.xy, Point::default()),
                        size: size(px(0.), px(0.)),
                    });
                }
            }
        }
    }
    union
}

/// Replay a guest display list into the host window. Coordinates on the wire are logical
/// pixels relative to the view's slot; the host adds the slot origin and paints through the
/// public `Window::paint_*` APIs (DESIGN.md invariant 5). Primitives are grouped by ascending
/// `order` and each group is painted inside its own `paint_layer` so guest stacking is
/// preserved (invariant 6).
fn replay(
    list: &bindings::DisplayList,
    origin: Point<Pixels>,
    clip: Bounds<Pixels>,
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
        window.paint_layer(clip, |window| {
            for &index in layer {
                paint_primitive(&list.primitives[index].prim, origin, clip, images, window);
            }
        });
        cursor = end;
    }
}

fn paint_primitive(
    primitive: &bindings::Primitive,
    slot_origin: Point<Pixels>,
    clip: Bounds<Pixels>,
    images: &std::collections::HashMap<u64, std::sync::Arc<gpui::RenderImage>>,
    window: &mut Window,
) {
    match primitive {
        bindings::Primitive::Quad(quad) => {
            let bounds = to_bounds(&quad.bounds, slot_origin);
            let mask = to_bounds(&quad.content_mask, slot_origin).intersect(&clip);
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
            let bounds = to_bounds(&shadow.bounds, slot_origin);
            let mask = to_bounds(&shadow.content_mask, slot_origin).intersect(&clip);
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
            let origin = to_point(&underline.origin, slot_origin);
            let mask = to_bounds(&underline.content_mask, slot_origin).intersect(&clip);
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
            let origin = to_point(&glyph.origin, slot_origin);
            let mask = to_bounds(&glyph.content_mask, slot_origin).intersect(&clip);
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
            let mask = to_bounds(&path.content_mask, slot_origin).intersect(&clip);
            let color = to_hsla(&path.color);
            let mut rebuilt = gpui::Path::new(to_point(&path.vertices[0].xy, slot_origin));
            for triangle in path.vertices.chunks_exact(3) {
                rebuilt.push_triangle(
                    (
                        to_point(&triangle[0].xy, slot_origin),
                        to_point(&triangle[1].xy, slot_origin),
                        to_point(&triangle[2].xy, slot_origin),
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
            let bounds = to_bounds(&image.bounds, slot_origin);
            let mask = to_bounds(&image.content_mask, slot_origin).intersect(&clip);
            let corner_radii = to_corners(&image.corner_radii);
            let render_image = render_image.clone();
            let grayscale = image.grayscale;
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                if let Err(error) =
                    window.paint_image(bounds, bounds, corner_radii, render_image, 0, grayscale)
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
