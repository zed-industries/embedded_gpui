//! The two well-known interfaces through which UI crosses the boundary, and the data
//! types their methods carry. Everything a host or guest knows about "views" is here,
//! as ordinary schemas — the WIT protocol has no notion of a view at all.
//!
//! - A [`SurfaceApi`] object is host-homed: a place where pixels go. The host creates
//!   one per slot in its element tree, shares it, and passes the ref to the plugin
//!   through whatever typed method the plugin's own schema defines. Display lists are
//!   addressed to it by object id.
//! - A [`ViewApi`] object is guest-homed: the thing drawing on a surface. The guest
//!   shares the view object and calls `surface.attach(view)`; the host then drives it
//!   with `resize`, `mouse`, and `key`. On the guest the view is a root of a window that
//!   mirrors the host window the surface is in (see [`Geometry`]).
//!
//! Both are ordinary objects, so every capability tool applies: an
//! `Attenuated<ViewApi>` allowing only `resize` is a display-only view, a `Revocable`
//! around a surface is a loan, and a plugin holding a surface ref can hand it to
//! another object without the host's involvement.
//!
//! Coordinates are logical pixels relative to the surface's origin.

use crate::{Ref, data, interface};

/// A place pixels go: one slot in the host's element tree.
#[interface]
pub trait SurfaceApi {
    /// Bind the view that will draw here. The host answers by driving the view's
    /// `resize` with the slot's current geometry, then forwards input to it. Attaching
    /// again replaces the previous view.
    fn attach(&mut self, view: Ref<ViewApi>, cx: &mut gpui::Context<Self>);

    /// The cursor to show while the pointer is over this surface.
    fn set_cursor(&mut self, cursor: Cursor, cx: &mut gpui::Context<Self>);
}

/// The thing drawing on a surface: a guest window.
#[interface]
pub trait ViewApi {
    /// The surface's slot moved or changed size, or its host window changed size, scale
    /// factor, or identity (also sent once on attach).
    fn resize(&mut self, geometry: Geometry, cx: &mut gpui::Context<Self>);

    fn mouse(&mut self, event: MouseEvent, cx: &mut gpui::Context<Self>);

    fn key(&mut self, event: KeyEvent, cx: &mut gpui::Context<Self>);
}

/// Where a surface's slot is: its bounds in a host window, and that window. The guest
/// keeps one window per host window it hears of, mirroring its size and scale factor,
/// and draws each view as a root of that window at the slot's real origin. So inside a
/// view, `window.viewport_size()` is the host's viewport, a popover clamps to the host
/// window's edges, and two views cannot overlap unless the host overlaps their slots.
#[data]
#[derive(Copy, PartialEq)]
pub struct Geometry {
    /// The slot's origin in the host window, logical pixels.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub window: HostWindow,
}

/// The host window a surface is in.
#[data]
#[derive(Copy, PartialEq)]
pub struct HostWindow {
    /// Stable for the window's lifetime and distinct among the host's windows: surfaces
    /// reporting the same id share one guest window.
    pub id: u64,
    /// The window's viewport, logical pixels.
    pub width: f32,
    pub height: f32,
    pub scale_factor: f32,
    /// Whether the window is the active (key) window.
    pub active: bool,
    pub appearance: Appearance,
}

/// Mirrors GPUI's `WindowAppearance`.
#[data]
#[derive(Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    VibrantLight,
    Dark,
    VibrantDark,
}

#[data]
#[derive(Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[data]
#[derive(Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

#[data]
#[derive(Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[data]
#[derive(PartialEq)]
pub struct MouseButtonEvent {
    pub button: MouseButton,
    pub position: Point,
    pub modifiers: Modifiers,
    pub click_count: u32,
}

#[data]
#[derive(PartialEq)]
pub struct MouseMoveEvent {
    pub position: Point,
    pub pressed_button: Option<MouseButton>,
    pub modifiers: Modifiers,
}

#[data]
#[derive(PartialEq)]
pub struct ScrollWheelEvent {
    pub position: Point,
    pub delta_x: f32,
    pub delta_y: f32,
    /// Whether the delta is in precise pixels (trackpad) or lines (wheel).
    pub precise: bool,
    pub modifiers: Modifiers,
}

/// The pointer left the surface (or the host window while over it). `position` is
/// where it was last seen, slot-relative and possibly outside the slot.
#[data]
#[derive(PartialEq)]
pub struct MouseExitEvent {
    pub position: Point,
    pub pressed_button: Option<MouseButton>,
    pub modifiers: Modifiers,
}

#[data]
#[derive(PartialEq)]
pub enum MouseEvent {
    Down(MouseButtonEvent),
    Up(MouseButtonEvent),
    Move(MouseMoveEvent),
    Scroll(ScrollWheelEvent),
    Exited(MouseExitEvent),
}

/// Mirrors GPUI's `Keystroke`.
#[data]
#[derive(PartialEq, Eq)]
pub struct Keystroke {
    pub modifiers: Modifiers,
    /// The character printed on the pressed key (e.g. "s", "enter", "backspace").
    pub key: String,
    /// The character this keystroke would type, if any (e.g. "s", or "ß" for option-s).
    pub key_char: Option<String>,
}

#[data]
#[derive(PartialEq, Eq)]
pub enum KeyEvent {
    Down {
        keystroke: Keystroke,
        is_held: bool,
    },
    Up {
        keystroke: Keystroke,
    },
    /// The held modifiers changed while the surface had focus.
    ModifiersChanged {
        modifiers: Modifiers,
    },
}

/// Mirrors GPUI's `CursorStyle` (subset).
#[data]
#[derive(Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    IBeam,
    Crosshair,
    ClosedHand,
    OpenHand,
    PointingHand,
    ResizeLeftRight,
    ResizeUpDown,
    OperationNotAllowed,
}

// ---------------------------------------------------------------------------------
// Conversions to and from GPUI's types. Both ends compile GPUI, so both directions
// live here: the host translates its element events into wire events, the guest
// translates wire events into `PlatformInput`.
// ---------------------------------------------------------------------------------

impl Point {
    pub fn from_gpui(point: gpui::Point<gpui::Pixels>) -> Self {
        Self {
            x: f32::from(point.x),
            y: f32::from(point.y),
        }
    }

    pub fn to_gpui(self) -> gpui::Point<gpui::Pixels> {
        gpui::point(gpui::px(self.x), gpui::px(self.y))
    }
}

impl MouseButton {
    /// `None` for buttons the protocol does not carry (navigate back/forward).
    pub fn from_gpui(button: gpui::MouseButton) -> Option<Self> {
        match button {
            gpui::MouseButton::Left => Some(Self::Left),
            gpui::MouseButton::Right => Some(Self::Right),
            gpui::MouseButton::Middle => Some(Self::Middle),
            gpui::MouseButton::Navigate(_) => None,
        }
    }

    pub fn to_gpui(self) -> gpui::MouseButton {
        match self {
            Self::Left => gpui::MouseButton::Left,
            Self::Right => gpui::MouseButton::Right,
            Self::Middle => gpui::MouseButton::Middle,
        }
    }
}

impl Modifiers {
    pub fn from_gpui(modifiers: gpui::Modifiers) -> Self {
        Self {
            control: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            platform: modifiers.platform,
        }
    }

    pub fn to_gpui(self) -> gpui::Modifiers {
        gpui::Modifiers {
            control: self.control,
            alt: self.alt,
            shift: self.shift,
            platform: self.platform,
            function: false,
        }
    }
}

impl Keystroke {
    pub fn from_gpui(keystroke: &gpui::Keystroke) -> Self {
        Self {
            modifiers: Modifiers::from_gpui(keystroke.modifiers),
            key: keystroke.key.clone(),
            key_char: keystroke.key_char.clone(),
        }
    }

    pub fn to_gpui(self) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers: self.modifiers.to_gpui(),
            key: self.key,
            key_char: self.key_char,
        }
    }
}

impl Appearance {
    pub fn from_gpui(appearance: gpui::WindowAppearance) -> Self {
        match appearance {
            gpui::WindowAppearance::Light => Self::Light,
            gpui::WindowAppearance::VibrantLight => Self::VibrantLight,
            gpui::WindowAppearance::Dark => Self::Dark,
            gpui::WindowAppearance::VibrantDark => Self::VibrantDark,
        }
    }

    pub fn to_gpui(self) -> gpui::WindowAppearance {
        match self {
            Self::Light => gpui::WindowAppearance::Light,
            Self::VibrantLight => gpui::WindowAppearance::VibrantLight,
            Self::Dark => gpui::WindowAppearance::Dark,
            Self::VibrantDark => gpui::WindowAppearance::VibrantDark,
        }
    }
}

impl MouseEvent {
    /// Translate a host element event into a wire event at `origin`-relative
    /// coordinates. `None` for buttons the protocol does not carry.
    pub fn from_gpui(
        input: &gpui::PlatformInput,
        origin: gpui::Point<gpui::Pixels>,
    ) -> Option<Self> {
        let event = match input {
            gpui::PlatformInput::MouseDown(event) => Self::Down(MouseButtonEvent {
                button: MouseButton::from_gpui(event.button)?,
                position: Point::from_gpui(event.position - origin),
                modifiers: Modifiers::from_gpui(event.modifiers),
                click_count: event.click_count as u32,
            }),
            gpui::PlatformInput::MouseUp(event) => Self::Up(MouseButtonEvent {
                button: MouseButton::from_gpui(event.button)?,
                position: Point::from_gpui(event.position - origin),
                modifiers: Modifiers::from_gpui(event.modifiers),
                click_count: event.click_count as u32,
            }),
            gpui::PlatformInput::MouseMove(event) => Self::Move(MouseMoveEvent {
                position: Point::from_gpui(event.position - origin),
                pressed_button: event.pressed_button.and_then(MouseButton::from_gpui),
                modifiers: Modifiers::from_gpui(event.modifiers),
            }),
            gpui::PlatformInput::ScrollWheel(event) => {
                let (delta_x, delta_y, precise) = match event.delta {
                    gpui::ScrollDelta::Pixels(delta) => {
                        (f32::from(delta.x), f32::from(delta.y), true)
                    }
                    gpui::ScrollDelta::Lines(delta) => (delta.x, delta.y, false),
                };
                Self::Scroll(ScrollWheelEvent {
                    position: Point::from_gpui(event.position - origin),
                    delta_x,
                    delta_y,
                    precise,
                    modifiers: Modifiers::from_gpui(event.modifiers),
                })
            }
            gpui::PlatformInput::MouseExited(event) => Self::Exited(MouseExitEvent {
                position: Point::from_gpui(event.position - origin),
                pressed_button: event.pressed_button.and_then(MouseButton::from_gpui),
                modifiers: Modifiers::from_gpui(event.modifiers),
            }),
            _ => return None,
        };
        Some(event)
    }

    pub fn to_platform_input(self) -> gpui::PlatformInput {
        match self {
            Self::Down(event) => gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: event.button.to_gpui(),
                position: event.position.to_gpui(),
                modifiers: event.modifiers.to_gpui(),
                click_count: event.click_count as usize,
                first_mouse: false,
            }),
            Self::Up(event) => gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: event.button.to_gpui(),
                position: event.position.to_gpui(),
                modifiers: event.modifiers.to_gpui(),
                click_count: event.click_count as usize,
            }),
            Self::Move(event) => gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: event.position.to_gpui(),
                pressed_button: event.pressed_button.map(MouseButton::to_gpui),
                modifiers: event.modifiers.to_gpui(),
            }),
            Self::Scroll(event) => gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: event.position.to_gpui(),
                delta: if event.precise {
                    gpui::ScrollDelta::Pixels(gpui::point(
                        gpui::px(event.delta_x),
                        gpui::px(event.delta_y),
                    ))
                } else {
                    gpui::ScrollDelta::Lines(gpui::point(event.delta_x, event.delta_y))
                },
                modifiers: event.modifiers.to_gpui(),
                touch_phase: Default::default(),
            }),
            Self::Exited(event) => gpui::PlatformInput::MouseExited(gpui::MouseExitEvent {
                position: event.position.to_gpui(),
                pressed_button: event.pressed_button.map(MouseButton::to_gpui),
                modifiers: event.modifiers.to_gpui(),
            }),
        }
    }
}

impl KeyEvent {
    pub fn from_gpui(input: &gpui::PlatformInput) -> Option<Self> {
        match input {
            gpui::PlatformInput::KeyDown(event) => Some(Self::Down {
                keystroke: Keystroke::from_gpui(&event.keystroke),
                is_held: event.is_held,
            }),
            gpui::PlatformInput::KeyUp(event) => Some(Self::Up {
                keystroke: Keystroke::from_gpui(&event.keystroke),
            }),
            gpui::PlatformInput::ModifiersChanged(event) => Some(Self::ModifiersChanged {
                modifiers: Modifiers::from_gpui(event.modifiers),
            }),
            _ => None,
        }
    }

    pub fn to_platform_input(self) -> gpui::PlatformInput {
        match self {
            Self::Down { keystroke, is_held } => gpui::PlatformInput::KeyDown(gpui::KeyDownEvent {
                keystroke: keystroke.to_gpui(),
                is_held,
                prefer_character_input: false,
            }),
            Self::Up { keystroke } => gpui::PlatformInput::KeyUp(gpui::KeyUpEvent {
                keystroke: keystroke.to_gpui(),
            }),
            Self::ModifiersChanged { modifiers } => {
                gpui::PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent {
                    modifiers: modifiers.to_gpui(),
                    capslock: Default::default(),
                })
            }
        }
    }
}

impl Cursor {
    pub fn from_gpui(style: gpui::CursorStyle) -> Self {
        use gpui::CursorStyle as G;
        match style {
            G::Arrow | G::ContextualMenu => Self::Arrow,
            G::IBeam | G::IBeamCursorForVerticalLayout => Self::IBeam,
            G::Crosshair => Self::Crosshair,
            G::ClosedHand => Self::ClosedHand,
            G::OpenHand => Self::OpenHand,
            G::PointingHand | G::DragLink | G::DragCopy => Self::PointingHand,
            G::ResizeLeft | G::ResizeRight | G::ResizeLeftRight | G::ResizeColumn => {
                Self::ResizeLeftRight
            }
            G::ResizeUp
            | G::ResizeDown
            | G::ResizeUpDown
            | G::ResizeRow
            | G::ResizeUpLeftDownRight
            | G::ResizeUpRightDownLeft => Self::ResizeUpDown,
            G::OperationNotAllowed => Self::OperationNotAllowed,
        }
    }

    pub fn to_gpui(self) -> gpui::CursorStyle {
        match self {
            Self::Arrow => gpui::CursorStyle::Arrow,
            Self::IBeam => gpui::CursorStyle::IBeam,
            Self::Crosshair => gpui::CursorStyle::Crosshair,
            Self::ClosedHand => gpui::CursorStyle::ClosedHand,
            Self::OpenHand => gpui::CursorStyle::OpenHand,
            Self::PointingHand => gpui::CursorStyle::PointingHand,
            Self::ResizeLeftRight => gpui::CursorStyle::ResizeLeftRight,
            Self::ResizeUpDown => gpui::CursorStyle::ResizeUpDown,
            Self::OperationNotAllowed => gpui::CursorStyle::OperationNotAllowed,
        }
    }
}
