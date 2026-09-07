//! The host-side [`Surface`]: the entity behind a `SurfaceApi` object. It caches the
//! guest's most recent display list and replays it every frame without calling into
//! the guest (DESIGN.md invariant 1), and drives the attached `ViewApi` with resize and
//! input as ordinary method calls.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, BoxShadow, ContentMask, Context, Corners, Edges, ElementInputHandler,
    EntityInputHandler, FocusHandle, IntoElement, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels,
    PlatformInput, Point, Render, ScrollWheelEvent, UTF16Selection, UnderlineStyle, Window, canvas,
    deferred, div, point, prelude::*, px,
};

use crate::host::{
    InputAnswer, InputQueries, InputQuery, KeyDownQuery, ReplaceAndMarkText, ReplaceText,
    TextRange, WireKeystroke, WireModifiers, bindings::Autocapitalize as WireAutocapitalize,
    bindings::TextInputAction as WireTextInputAction,
};
use crate::surface::{
    Appearance, Cursor, Geometry, HostWindow, KeyEvent, MouseEvent, SurfaceApi, ViewApi,
    ViewApiCaller as _,
};
use crate::{PluginImages, Ref, Remote, bindings};

/// Overlays are deferred above everything the host defers itself.
const OVERLAY_PRIORITY: usize = 1 << 20;

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
    /// above the whole host window.
    overlay: Option<(bindings::DisplayList, PluginImages)>,
    cursor: Option<gpui::CursorStyle>,
    geometry: Option<Geometry>,
    last_origin: Point<Pixels>,
    focus_handle: FocusHandle,
    /// The synchronous channel into the plugin the view lives in: key precedence and
    /// the IME's questions go through it (see [`InputQueries`]).
    queries: Option<Rc<InputQueries>>,
    /// Why the plugin behind the view stopped, shown in place of the scene.
    stopped: Option<String>,
    /// Set by a mouse-down that is about to focus the surface, so the focus-in that
    /// follows is not mistaken for keyboard traversal entering it.
    focus_by_pointer: bool,
    focus_in: Option<gpui::Subscription>,
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
            // A tab stop of the host's, so the host's own traversal reaches the surface.
            focus_handle: cx.focus_handle().tab_stop(true),
            queries: None,
            stopped: None,
            focus_by_pointer: false,
            focus_in: None,
        }
    }

    /// Keyboard focus reached the surface (by traversal, not by a click): tell the view
    /// to focus its first stop, or its last if the traversal was backward (Shift held).
    fn focus_gained(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.focus_by_pointer) {
            return;
        }
        if let Some(view) = &self.view {
            view.focus_entered(window.modifiers().shift, cx);
        }
    }

    pub(crate) fn set_stopped(&mut self, reason: String, cx: &mut Context<Self>) {
        self.stopped = Some(reason);
        self.overlay = None;
        cx.notify();
    }

    /// Ask the plugin something the host cannot wait a turn for. `None` when there is no
    /// view, no channel, no focused text field on the guest, or no answer in time.
    fn query(&self, query: InputQuery) -> Option<InputAnswer> {
        let view = self.view.as_ref()?.reference().entity_id();
        let answer = self.queries.as_ref()?.query(view, query)?;
        match answer {
            InputAnswer::None => None,
            answer => Some(answer),
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
        self.overlay = (!list.primitives.is_empty()).then_some((list, images));
        cx.notify();
    }

    /// Mouse listeners forwarding to the view, for the slot and for its overlay alike.
    /// Positions are made slot-relative; the guest puts them back into window
    /// coordinates, so an overlay outside the slot works the same way.
    fn wire_mouse<E: StatefulInteractiveElement>(&self, element: E, cx: &mut Context<Self>) -> E {
        element
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                if !this.focus_handle.is_focused(window) {
                    this.focus_by_pointer = true;
                    window.focus(&this.focus_handle, cx);
                }
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

    /// Key-downs are the one input that needs an answer: whether the guest consumed the
    /// key decides whether the host platform turns it into text (through the IME path,
    /// which lands in [`EntityInputHandler::replace_text_in_range`] below).
    fn key_down(&self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // A paste reads the clipboard synchronously on the guest, from a copy the host's
        // clipboard object keeps fresh: refreshing it here sends any change as an event
        // that reaches the guest ahead of this key.
        let modifiers = event.keystroke.modifiers;
        if (modifiers.platform || modifiers.control || modifiers.alt)
            && let Some(queries) = &self.queries
        {
            queries.refresh_clipboard(cx);
        }
        let query = InputQuery::KeyDown(KeyDownQuery {
            keystroke: WireKeystroke {
                modifiers: WireModifiers {
                    control: event.keystroke.modifiers.control,
                    alt: event.keystroke.modifiers.alt,
                    shift: event.keystroke.modifiers.shift,
                    platform: event.keystroke.modifiers.platform,
                    function: event.keystroke.modifiers.function,
                },
                key: event.keystroke.key.clone(),
                key_char: event.keystroke.key_char.clone(),
            },
            is_held: event.is_held,
        });
        if let Some(InputAnswer::Handled(true)) = self.query(query) {
            cx.stop_propagation();
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
        self.queries = view.registry().extension::<InputQueries>();
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
        let focus_handle = self.focus_handle.clone();
        let overlay_regions: Option<Vec<(Bounds<Pixels>, bool)>> =
            self.overlay.as_ref().map(|(list, _)| {
                list.hit_regions
                    .iter()
                    .map(|region| {
                        (
                            to_bounds(&region.bounds, Point::default()),
                            region.block_mouse,
                        )
                    })
                    .collect()
            });

        let slot = div()
            .size_full()
            .id(("embedded-surface", cx.entity_id()))
            .track_focus(&self.focus_handle)
            .when_some(self.cursor, |this, cursor| this.cursor(cursor))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.key_down(event, cx);
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

        let overlay = overlay_regions.map(|regions| {
            // The overlay paints from a zero-sized anchor at the slot's origin, and one
            // input region per guest hitbox sits where the overlay's elements are:
            // clicks reach the view there and pass through to the host elsewhere.
            let mut anchor = div()
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .w(px(0.))
                .h(px(0.))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                            let surface = overlay_entity.read(cx);
                            if let Some((list, images)) = surface.overlay.as_ref() {
                                let images = images.borrow();
                                let clip = Bounds {
                                    origin: Point::default(),
                                    size: window.viewport_size(),
                                };
                                replay(list, surface.last_origin, clip, &images, window);
                            }
                        },
                    )
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w(px(1.))
                    .h(px(1.)),
                );
            for (index, (bounds, block_mouse)) in regions.into_iter().enumerate() {
                let region = div()
                    .id(("embedded-overlay-region", index))
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y)
                    .w(bounds.size.width)
                    .h(bounds.size.height)
                    .when(block_mouse, |this| this.occlude());
                anchor = anchor.child(self.wire_mouse(region, cx));
            }
            deferred(anchor).with_priority(OVERLAY_PRIORITY)
        });

        let stopped = self.stopped.clone().map(|reason| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.6))
                .text_color(gpui::white())
                .text_size(px(12.))
                .child(reason)
        });
        slot.child(
            canvas(
                move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                    prepaint_entity.update(cx, |this, cx| {
                        this.last_origin = bounds.origin;
                        this.measured(bounds, window, cx);
                        if this.focus_in.is_none() {
                            let handle = this.focus_handle.clone();
                            this.focus_in =
                                Some(cx.on_focus_in(&handle, window, |this, window, cx| {
                                    this.focus_gained(window, cx)
                                }));
                        }
                    });
                    bounds
                },
                move |bounds: Bounds<Pixels>,
                      _: Bounds<Pixels>,
                      window: &mut Window,
                      cx: &mut App| {
                    // The IME talks to this surface as it would to a text field; the
                    // answers come from the guest's focused field over the query channel.
                    window.handle_input(
                        &focus_handle,
                        ElementInputHandler::new(bounds, paint_entity.clone()),
                        cx,
                    );
                    let surface = paint_entity.read(cx);
                    if let Some((list, images)) = surface.display_list.as_ref() {
                        let images = images.borrow();
                        replay(list, bounds.origin, bounds, &images, window);
                    }
                },
            )
            .size_full(),
        )
        .children(stopped)
        .children(overlay)
    }
}

fn wire_range(range: Range<usize>) -> TextRange {
    TextRange {
        start: range.start as u32,
        end: range.end as u32,
    }
}

fn range_from_wire(range: TextRange) -> Range<usize> {
    range.start as usize..range.end as usize
}

/// The host's IME sees a surface as a text field; every question is relayed to the
/// guest's focused field synchronously (see [`InputQueries`]).
impl EntityInputHandler for Surface {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        match self.query(InputQuery::TextForRange(wire_range(range)))? {
            InputAnswer::Text(answer) => {
                *adjusted_range = answer.adjusted.map(range_from_wire);
                Some(answer.text)
            }
            _ => None,
        }
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        match self.query(InputQuery::SelectedTextRange(ignore_disabled_input))? {
            InputAnswer::Selection(selection) => Some(UTF16Selection {
                range: range_from_wire(selection.range),
                reversed: selection.reversed,
            }),
            _ => None,
        }
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        match self.query(InputQuery::MarkedTextRange)? {
            InputAnswer::Range(range) => Some(range_from_wire(range)),
            _ => None,
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.query(InputQuery::UnmarkText);
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.query(InputQuery::ReplaceTextInRange(ReplaceText {
            range: range.map(wire_range),
            text: text.to_string(),
        }));
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.query(InputQuery::ReplaceAndMarkTextInRange(ReplaceAndMarkText {
            range: range.map(wire_range),
            text: new_text.to_string(),
            new_selected_range: new_selected_range.map(wire_range),
        }));
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        match self.query(InputQuery::BoundsForRange(wire_range(range_utf16)))? {
            InputAnswer::Bounds(bounds) => Some(to_bounds(&bounds, self.last_origin)),
            _ => None,
        }
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = point - self.last_origin;
        let query = InputQuery::CharacterIndexForPoint(bindings::Point {
            x: f32::from(local.x),
            y: f32::from(local.y),
        });
        match self.query(query)? {
            InputAnswer::Index(index) => Some(index as usize),
            _ => None,
        }
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.query(InputQuery::SetSelectedTextRange(wire_range(range_utf16)));
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        match self.query(InputQuery::TextLength)? {
            InputAnswer::Index(length) => Some(length as usize),
            _ => None,
        }
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        matches!(
            self.query(InputQuery::AcceptsTextInput),
            Some(InputAnswer::Accepts(true))
        )
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::TextInputConfiguration {
        let Some(InputAnswer::Configuration(configuration)) =
            self.query(InputQuery::TextInputConfiguration)
        else {
            return gpui::TextInputConfiguration::default();
        };
        gpui::TextInputConfiguration {
            autocorrect: configuration.autocorrect,
            autocapitalize: match configuration.autocapitalize {
                WireAutocapitalize::None => gpui::Autocapitalize::None,
                WireAutocapitalize::Words => gpui::Autocapitalize::Words,
                WireAutocapitalize::Sentences => gpui::Autocapitalize::Sentences,
                WireAutocapitalize::Characters => gpui::Autocapitalize::Characters,
            },
            suggestions: configuration.suggestions,
            input_action: match configuration.input_action {
                WireTextInputAction::Unspecified => gpui::TextInputAction::Unspecified,
                WireTextInputAction::Enter => gpui::TextInputAction::Enter,
                WireTextInputAction::Done => gpui::TextInputAction::Done,
                WireTextInputAction::Go => gpui::TextInputAction::Go,
                WireTextInputAction::Next => gpui::TextInputAction::Next,
                WireTextInputAction::Previous => gpui::TextInputAction::Previous,
                WireTextInputAction::Search => gpui::TextInputAction::Search,
                WireTextInputAction::Send => gpui::TextInputAction::Send,
            },
        }
    }

    fn text_input_editable_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        match self.query(InputQuery::TextInputEditableRange)? {
            InputAnswer::Range(range) => Some(range_from_wire(range)),
            _ => None,
        }
    }
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
