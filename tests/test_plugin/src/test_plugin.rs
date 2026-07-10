//! The guest half of `tests/shared_entities.rs`: a zoo of guest-homed
//! entities exercising calls, events, capability refs, attenuation, and fully dynamic
//! dispatch.

use anyhow::anyhow;
use embedded_gpui::shared::{connect, share, share_anonymous, share_anonymous_with, share_with};
use embedded_gpui::{Plugin, SharedRef, decode, encode, register_plugin, shared};
use embedded_gpui_util::Revocable;
use gpui::{AnyView, App, Context, Entity, EventEmitter, Task, Window, div, prelude::*};
use test_schema::{
    ChameleonApi, ChameleonState, CounterMilestone, FactoryApi, GatekeeperApi, ItemApi, ItemInfo,
    TestCounterApi, VaultApi,
};

/// Named shares borrow their entities (the sharer owns the lifetime), so the plugin must
/// keep them alive; anonymous shares own theirs until released.
struct TestGuest {
    _counter: Entity<Counter>,
    _factory: Entity<Factory>,
    _chameleon: Entity<Chameleon>,
    _gatekeeper: Entity<Gatekeeper>,
}

impl Plugin for TestGuest {
    fn new(cx: &mut App) -> Self {
        let counter = cx.new(|_| Counter { count: 0 });
        share(&counter, "guest-counter", cx);

        let factory = cx.new(|_| Factory { created: 0 });
        share(&factory, "factory", cx);

        let gatekeeper = cx.new(|_| Gatekeeper { guarded: 0 });
        share(&gatekeeper, "gatekeeper", cx);

        let chameleon = cx.new(|_| Chameleon {
            mode: "echo".to_string(),
            pokes: 0,
        });
        // Entirely dynamic dispatch: one wildcard handler interprets every method name at
        // runtime and can change its own behavior ("become"). The schema declares nothing
        // but the wire name, so this uses the closure escape hatch under `share`.
        share_with::<ChameleonApi, _>(
            &chameleon,
            "chameleon",
            |methods| {
                methods.on("*", |entity, method, payload, cx| {
                    entity.update(cx, |this, cx| match method {
                        "become" => {
                            this.mode = decode(payload)?;
                            cx.notify();
                            encode(&())
                        }
                        "poke" => {
                            this.pokes += 1;
                            cx.notify();
                            let input: String = decode(payload)?;
                            match this.mode.as_str() {
                                "echo" => encode(&input),
                                "shout" => encode(&input.to_uppercase()),
                                "reverse" => encode(&input.chars().rev().collect::<String>()),
                                other => Err(anyhow!("chameleon has no mode {other:?}")),
                            }
                        }
                        "state" => encode(&ChameleonState {
                            mode: this.mode.clone(),
                            pokes: this.pokes,
                        }),
                        other => Err(anyhow!("chameleon does not understand {other:?}")),
                    })
                });
            },
            cx,
        );

        TestGuest {
            _counter: counter,
            _factory: factory,
            _chameleon: chameleon,
            _gatekeeper: gatekeeper,
        }
    }

    fn create_view(&mut self, _name: &str, _window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| EmptyView).into()
    }
}

register_plugin!(TestGuest);

struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct Counter {
    count: u32,
}

impl EventEmitter<CounterMilestone> for Counter {}

#[shared]
impl TestCounterApi for Counter {
    fn increment(&mut self, by: u32, cx: &mut Context<Self>) -> u32 {
        let tens_before = self.count / 10;
        self.count += by;
        if self.count / 10 > tens_before {
            cx.emit(CounterMilestone { count: self.count });
        }
        cx.notify();
        self.count
    }

    fn count(&mut self, _cx: &mut Context<Self>) -> u32 {
        self.count
    }
}

struct Item {
    label: String,
    bumps: u32,
}

#[shared]
impl ItemApi for Item {
    fn bump(&mut self, cx: &mut Context<Self>) -> u32 {
        self.bumps += 1;
        cx.notify();
        self.bumps
    }

    fn describe(&mut self, _cx: &mut Context<Self>) -> ItemInfo {
        ItemInfo {
            label: self.label.clone(),
            bumps: self.bumps,
        }
    }
}

struct Factory {
    created: u32,
}

#[shared]
impl FactoryApi for Factory {
    fn create(&mut self, label: String, cx: &mut Context<Self>) -> SharedRef<ItemApi> {
        self.created += 1;
        cx.notify();
        let item: Entity<Item> = cx.new(|_| Item { label, bumps: 0 });
        share_anonymous(&item, cx)
    }
}

struct Gatekeeper {
    guarded: u32,
}

#[shared]
impl GatekeeperApi for Gatekeeper {
    fn guard(&mut self, vault: SharedRef<VaultApi>, cx: &mut Context<Self>) -> SharedRef<VaultApi> {
        self.guarded += 1;
        cx.notify();
        // The membrane is the stock caretaker from embedded_gpui_util: every method
        // forwards to the wrapped vault capability, and revoking drops the inner remote
        // (auto-release cascades to the vault's home).
        let vault = connect(vault, cx);
        let revocable = Revocable::new(vault, cx);
        share_anonymous_with(
            &revocable,
            |methods| {
                Revocable::register(methods);
                // Revocation authority is a deliberate grant: this guest chooses to let
                // its peer revoke over the wire.
                methods.on("revoke", |entity, _method, _payload, cx| {
                    entity.update(cx, |revocable, cx| revocable.revoke(cx));
                    encode(&())
                });
            },
            cx,
        )
    }

    fn probe(
        &mut self,
        target: SharedRef<ItemApi>,
        method: String,
        payload: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Vec<u8>>> {
        let remote = connect(target, cx);
        let receipt = remote.forward(&method, payload, cx);
        cx.spawn(async move |_, _| {
            let outcome = receipt.await;
            drop(remote);
            outcome
        })
    }
}

struct Chameleon {
    mode: String,
    pokes: u32,
}
