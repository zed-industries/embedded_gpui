//! Schemas for the integration tests in `tests`.

use embedded_gpui::{SharedRef, shared_data, shared_interface};

/// The guest-homed counter driven from the host: reads are calls (`count`), and every
/// crossing of a multiple of ten emits a [`CounterMilestone`] event.
#[shared_interface("test.counter", events = [CounterMilestone])]
pub trait TestCounterApi {
    fn increment(&mut self, by: u32, cx: &mut gpui::Context<Self>) -> u32;
    fn count(&mut self, cx: &mut gpui::Context<Self>) -> u32;
}

/// Emitted by the counter's home when the count crosses a multiple of ten.
#[shared_data]
pub struct CounterMilestone {
    pub count: u32,
}

#[shared_interface("test.item")]
pub trait ItemApi {
    fn bump(&mut self, cx: &mut gpui::Context<Self>) -> u32;
    fn describe(&mut self, cx: &mut gpui::Context<Self>) -> ItemInfo;
}

#[shared_data]
pub struct ItemInfo {
    pub label: String,
    pub bumps: u32,
}

#[shared_interface("test.factory")]
pub trait FactoryApi {
    fn create(&mut self, label: String, cx: &mut gpui::Context<Self>) -> SharedRef<ItemApi>;
}

/// Declared with an `async fn`: the home implements it as a method returning a
/// `Task<Result<String>>`, and the response flows when the task resolves.
#[shared_interface("test.vault")]
pub trait VaultApi {
    async fn read(&mut self, cx: &mut gpui::Context<Self>) -> String;
}

#[shared_interface("test.gatekeeper")]
pub trait GatekeeperApi {
    /// Wrap the given vault capability in a guest-side caretaker and return a ref to
    /// *that*; the caller can't tell the difference.
    fn guard(
        &mut self,
        vault: SharedRef<VaultApi>,
        cx: &mut gpui::Context<Self>,
    ) -> SharedRef<VaultApi>;

    /// Call an arbitrary method on an arbitrary item capability from the guest side, so
    /// tests can verify what a ref does and does not permit from across the boundary.
    async fn probe(
        &mut self,
        target: SharedRef<ItemApi>,
        method: String,
        payload: Vec<u8>,
        cx: &mut gpui::Context<Self>,
    ) -> Vec<u8>;
}

/// No methods at all: the chameleon is shared with `share_with` and interprets its
/// method names at runtime, so only the wire name exists in the schema.
#[shared_interface("test.chameleon")]
pub trait ChameleonApi {}

/// What the chameleon's dynamic "state" method returns.
#[shared_data]
pub struct ChameleonState {
    pub mode: String,
    pub pokes: u32,
}
