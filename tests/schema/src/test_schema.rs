//! Schemas for the integration tests in `tests`.
//!
//! The two root interfaces are the whole bootstrap: each end installs its root at its
//! id 0, and every other capability below is reached by calling root methods that
//! return refs.

use embedded_gpui::surface::SurfaceApi;
use embedded_gpui::{Ref, data, interface};

/// The plugin's root object: the host's entire view of the plugin. The methods create
/// their entities lazily on first call and return the same ref thereafter.
#[interface]
pub trait TestPlugin {
    fn counter(&mut self, cx: &mut gpui::Context<Self>) -> Ref<TestCounterApi>;
    fn factory(&mut self, cx: &mut gpui::Context<Self>) -> Ref<FactoryApi>;
    fn gatekeeper(&mut self, cx: &mut gpui::Context<Self>) -> Ref<GatekeeperApi>;
    fn chameleon(&mut self, cx: &mut gpui::Context<Self>) -> Ref<ChameleonApi>;

    /// Calls `ping` on the host's root and relays the reply: the bootstrap exercised
    /// in the other direction, from inside a handler.
    async fn ping_host(&mut self, message: String, cx: &mut gpui::Context<Self>) -> String;

    /// Open a view on a host surface and return a probe into it: views are objects, so
    /// the whole UI path is exercised as ordinary method calls.
    fn mount(
        &mut self,
        surface: Ref<SurfaceApi>,
        cx: &mut gpui::Context<Self>,
    ) -> Ref<ViewProbeApi>;

    /// Busy-loop for the given wall-clock time: a misbehaving plugin, for the host's
    /// turn budget to catch.
    fn spin(&mut self, millis: u64, cx: &mut gpui::Context<Self>);
}

/// What a view can see of where it is: its bounds in its window, the window's viewport
/// (the host window's, mirrored), and the scale factor it renders at.
#[data]
#[derive(Copy, PartialEq)]
pub struct SeenGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub scale_factor: f32,
    pub active: bool,
}

/// What the plugin observed of a mounted view.
#[interface]
pub trait ViewProbeApi {
    /// Where the view found itself when it last laid out, if it has.
    fn last_geometry(&mut self, cx: &mut gpui::Context<Self>) -> Option<SeenGeometry>;
    /// Mouse-down events the view's root element received.
    fn clicks(&mut self, cx: &mut gpui::Context<Self>) -> u32;
    /// Focus the view's text field, so the host's input queries reach it.
    fn focus(&mut self, cx: &mut gpui::Context<Self>);
    /// The text field's contents.
    fn text(&mut self, cx: &mut gpui::Context<Self>) -> String;
    /// Key-downs the view's root element handled (every "enter").
    fn handled_keys(&mut self, cx: &mut gpui::Context<Self>) -> u32;
    /// Whether the window's root view still exists (it dies when the host drops the
    /// surface and the view is released).
    fn view_alive(&mut self, cx: &mut gpui::Context<Self>) -> bool;
}

/// The host's root object: everything this suite's plugin can reach on the host.
#[interface]
pub trait TestHost {
    fn ping(&mut self, message: String, cx: &mut gpui::Context<Self>) -> String;
}

/// The plugin-homed counter driven from the host: reads are calls (`count`), and every
/// crossing of a multiple of ten emits a [`CounterMilestone`] event.
#[interface(events = [CounterMilestone])]
pub trait TestCounterApi {
    fn increment(&mut self, by: u32, cx: &mut gpui::Context<Self>) -> u32;
    fn count(&mut self, cx: &mut gpui::Context<Self>) -> u32;
}

/// Emitted by the counter's home when the count crosses a multiple of ten.
#[data]
pub struct CounterMilestone {
    pub count: u32,
}

#[interface]
pub trait ItemApi {
    fn bump(&mut self, cx: &mut gpui::Context<Self>) -> u32;
    fn describe(&mut self, cx: &mut gpui::Context<Self>) -> ItemInfo;
}

#[data]
pub struct ItemInfo {
    pub label: String,
    pub bumps: u32,
}

#[interface]
pub trait FactoryApi {
    fn create(&mut self, label: String, cx: &mut gpui::Context<Self>) -> Ref<ItemApi>;
}

/// Declared with an `async fn`: the home implements it as a method returning a
/// `Task<Result<String>>`, and the response flows when the task resolves.
#[interface]
pub trait VaultApi {
    async fn read(&mut self, cx: &mut gpui::Context<Self>) -> String;

    /// A capability the vault hands out: through a membrane, the returned ref is itself
    /// wrapped, so revoking the membrane revokes the key too.
    fn key(&mut self, cx: &mut gpui::Context<Self>) -> Ref<KeyApi>;
}

#[interface]
pub trait KeyApi {
    fn unlock(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

#[interface]
pub trait GatekeeperApi {
    /// Wrap the given vault capability in a guest-side caretaker and return a ref to
    /// *that*; the caller can't tell the difference.
    fn guard(&mut self, vault: Ref<VaultApi>, cx: &mut gpui::Context<Self>) -> Ref<VaultApi>;

    /// Call an arbitrary method on an arbitrary item capability from the guest side, so
    /// tests can verify what a ref does and does not permit from across the boundary.
    async fn probe(
        &mut self,
        target: Ref<ItemApi>,
        method: String,
        payload: Vec<u8>,
        cx: &mut gpui::Context<Self>,
    ) -> Vec<u8>;
}

/// No methods at all: the chameleon is shared with `share_with` and interprets its
/// method names at runtime, so the schema declares nothing but the interface itself.
#[interface]
pub trait ChameleonApi {}

/// What the chameleon's dynamic "state" method returns.
#[data]
pub struct ChameleonState {
    pub mode: String,
    pub pokes: u32,
}
