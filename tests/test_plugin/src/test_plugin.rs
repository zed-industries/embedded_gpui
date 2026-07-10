//! The plugin half of `tests/shared_entities.rs`: one root object through which the
//! host reaches a zoo of entities exercising calls, events, capability refs,
//! attenuation, and fully dynamic dispatch.

use anyhow::anyhow;
use embedded_gpui::{
    Plugin, Remote, SharedRef, connect, decode, encode, register_plugin, remote_root, share,
    share_root, share_with, shared,
};
use embedded_gpui_util::Revocable;
use gpui::{AnyView, App, Context, Entity, EventEmitter, Task, Window, div, prelude::*};
use test_schema::{
    ChameleonApi, ChameleonState, CounterMilestone, FactoryApi, GatekeeperApi, ItemApi, ItemInfo,
    TestCounterApi, TestHost, TestHostCaller as _, TestPlugin, VaultApi,
};

/// The plugin's whole bootstrap: construct the root object and install it at this end's
/// id 0. Everything else is reached through the root's methods.
struct TestGuest {
    _root: Entity<Root>,
}

impl Plugin for TestGuest {
    fn new(cx: &mut App) -> Self {
        let host = remote_root::<TestHost>(cx);
        let root = cx.new(|_| Root {
            host,
            counter: None,
            factory: None,
            gatekeeper: None,
            chameleon: None,
        });
        share_root(&root, cx);
        TestGuest { _root: root }
    }

    fn create_view(&mut self, _name: &str, _window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| EmptyView).into()
    }
}

register_plugin!(TestGuest);

/// The root object: each method is a lazy factory, creating its entity on first call
/// and returning the same ref thereafter. The root keeps the entities alive itself so a
/// release (last remote dropped on the other end) does not invalidate the cached ref.
struct Root {
    host: Remote<TestHost>,
    counter: Option<(Entity<Counter>, SharedRef<TestCounterApi>)>,
    factory: Option<(Entity<Factory>, SharedRef<FactoryApi>)>,
    gatekeeper: Option<(Entity<Gatekeeper>, SharedRef<GatekeeperApi>)>,
    chameleon: Option<(Entity<Chameleon>, SharedRef<ChameleonApi>)>,
}

#[shared]
impl TestPlugin for Root {
    fn counter(&mut self, cx: &mut Context<Self>) -> SharedRef<TestCounterApi> {
        if let Some((_, reference)) = &self.counter {
            return *reference;
        }
        let counter = cx.new(|_| Counter { count: 0 });
        let reference = share(&counter, cx);
        self.counter = Some((counter, reference));
        reference
    }

    fn factory(&mut self, cx: &mut Context<Self>) -> SharedRef<FactoryApi> {
        if let Some((_, reference)) = &self.factory {
            return *reference;
        }
        let factory = cx.new(|_| Factory { created: 0 });
        let reference = share(&factory, cx);
        self.factory = Some((factory, reference));
        reference
    }

    fn gatekeeper(&mut self, cx: &mut Context<Self>) -> SharedRef<GatekeeperApi> {
        if let Some((_, reference)) = &self.gatekeeper {
            return *reference;
        }
        let gatekeeper = cx.new(|_| Gatekeeper { guarded: 0 });
        let reference = share(&gatekeeper, cx);
        self.gatekeeper = Some((gatekeeper, reference));
        reference
    }

    fn chameleon(&mut self, cx: &mut Context<Self>) -> SharedRef<ChameleonApi> {
        if let Some((_, reference)) = &self.chameleon {
            return *reference;
        }
        let chameleon = cx.new(|_| Chameleon {
            mode: "echo".to_string(),
            pokes: 0,
        });
        // Entirely dynamic dispatch: one wildcard handler interprets every method name at
        // runtime and can change its own behavior ("become"). The schema declares nothing
        // but the interface, so this uses the closure escape hatch under `share`.
        let reference = share_with::<ChameleonApi, _>(
            &chameleon,
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
        self.chameleon = Some((chameleon, reference));
        reference
    }

    fn ping_host(
        &mut self,
        message: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<String>> {
        // The symmetric bootstrap from inside a handler: this end's remote to the other
        // end's root, used like any other capability.
        let receipt = self.host.ping(message, cx);
        cx.spawn(async move |_, _| receipt.await)
    }
}

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
        share(&item, cx)
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
        share_with(
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
