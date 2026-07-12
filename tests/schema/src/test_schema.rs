//! Schemas for the integration tests in `tests`.
//!
//! The two root interfaces are the whole bootstrap: each end installs its root at its
//! id 0, and every other capability below is reached by calling root methods that
//! return refs.

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
