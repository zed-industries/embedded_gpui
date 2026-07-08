//! The guest half of `tests/shared_entities.rs`: a zoo of guest-homed
//! entities exercising calls, capability refs, attenuation, and fully dynamic dispatch.

use anyhow::anyhow;
use embedded_gpui::shared::{HandleShared, SharedEntitySource, SharedRef};
use embedded_gpui::test_schema::{
    Bump, ChameleonSnapshot, ChameleonSpec, CreateItem, FactorySnapshot, FactorySpec,
    GatekeeperSnapshot, GatekeeperSpec, Guard, ItemSnapshot, ItemSpec, TestCounterSnapshot,
    TestCounterSpec, TestIncrement, VaultSnapshot, VaultSpec,
};
use embedded_gpui::{Plugin, register_plugin};
use embedded_gpui::{decode, encode};
use embedded_gpui_util::Revocable;
use gpui::{AnyView, App, Context, Entity, Window, div, prelude::*};

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
        embedded_gpui::shared::share::<TestCounterSpec, _>(
            &counter,
            "guest-counter",
            |methods| {
                methods.on::<TestIncrement>();
            },
            cx,
        );

        let factory = cx.new(|_| Factory { created: 0 });
        embedded_gpui::shared::share::<FactorySpec, _>(
            &factory,
            "factory",
            |methods| {
                methods.on::<CreateItem>();
            },
            cx,
        );

        let gatekeeper = cx.new(|_| Gatekeeper { guarded: 0 });
        embedded_gpui::shared::share::<GatekeeperSpec, _>(
            &gatekeeper,
            "gatekeeper",
            |methods| {
                methods.on::<Guard>();
            },
            cx,
        );

        let chameleon = cx.new(|_| Chameleon {
            mode: "echo".to_string(),
            pokes: 0,
        });
        // Entirely dynamic dispatch: one wildcard handler interprets every method name at
        // runtime and can change its own behavior ("become").
        embedded_gpui::shared::share::<ChameleonSpec, _>(
            &chameleon,
            "chameleon",
            |methods| {
                methods.on_raw("*", |entity, method, payload, cx| {
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

    fn create_view(&mut self, _view_id: u32, _window: &mut Window, cx: &mut App) -> AnyView {
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

impl SharedEntitySource<TestCounterSpec> for Counter {
    fn snapshot(&self, _cx: &App) -> TestCounterSnapshot {
        TestCounterSnapshot { count: self.count }
    }
}

impl HandleShared<TestIncrement> for Counter {
    fn handle(&mut self, message: TestIncrement, cx: &mut Context<Self>) -> u32 {
        self.count += message.by;
        cx.notify();
        self.count
    }
}

struct Factory {
    created: u32,
}

impl SharedEntitySource<FactorySpec> for Factory {
    fn snapshot(&self, _cx: &App) -> FactorySnapshot {
        FactorySnapshot {
            created: self.created,
        }
    }
}

struct Item {
    label: String,
    bumps: u32,
}

impl SharedEntitySource<ItemSpec> for Item {
    fn snapshot(&self, _cx: &App) -> ItemSnapshot {
        ItemSnapshot {
            label: self.label.clone(),
            bumps: self.bumps,
        }
    }
}

impl HandleShared<Bump> for Item {
    fn handle(&mut self, _message: Bump, cx: &mut Context<Self>) -> u32 {
        self.bumps += 1;
        cx.notify();
        self.bumps
    }
}

impl HandleShared<CreateItem> for Factory {
    fn handle(&mut self, message: CreateItem, cx: &mut Context<Self>) -> SharedRef<ItemSpec> {
        self.created += 1;
        cx.notify();
        let item: Entity<Item> = cx.new(|_| Item {
            label: message.label,
            bumps: 0,
        });
        embedded_gpui::shared::share_anonymous::<ItemSpec, _>(
            &item,
            |methods| {
                methods.on::<Bump>();
            },
            cx,
        )
    }
}

struct Gatekeeper {
    guarded: u32,
}

impl SharedEntitySource<GatekeeperSpec> for Gatekeeper {
    fn snapshot(&self, _cx: &App) -> GatekeeperSnapshot {
        GatekeeperSnapshot {
            guarded: self.guarded,
        }
    }
}

impl HandleShared<Guard> for Gatekeeper {
    fn handle(&mut self, message: Guard, cx: &mut Context<Self>) -> SharedRef<VaultSpec> {
        self.guarded += 1;
        cx.notify();
        // The membrane is the stock caretaker from embedded_gpui_util: snapshots pass
        // through, every method forwards to the wrapped vault capability, and revoking
        // drops the inner remote (auto-release cascades to the vault's home).
        let vault = embedded_gpui::shared::remote_from_ref::<VaultSpec>(message.vault, cx);
        let revocable = Revocable::new(
            vault,
            VaultSnapshot {
                label: "pending".to_string(),
            },
            cx,
        );
        embedded_gpui::shared::share_anonymous::<VaultSpec, _>(
            &revocable,
            |methods| {
                Revocable::register(methods);
                // Revocation authority is a deliberate grant: this guest chooses to let
                // its peer revoke over the wire.
                methods.on_raw("revoke", |entity, _method, _payload, cx| {
                    entity.update(cx, |revocable, cx| revocable.revoke(cx));
                    encode(&())
                });
            },
            cx,
        )
    }
}

struct Chameleon {
    mode: String,
    pokes: u32,
}

impl SharedEntitySource<ChameleonSpec> for Chameleon {
    fn snapshot(&self, _cx: &App) -> ChameleonSnapshot {
        ChameleonSnapshot {
            mode: self.mode.clone(),
            pokes: self.pokes,
        }
    }
}
