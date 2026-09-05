//! The plugin half of `tests/shared_entities.rs`: one root object through which the
//! host reaches a zoo of entities exercising calls, events, capability refs,
//! attenuation, and fully dynamic dispatch.

use anyhow::anyhow;
use embedded_gpui::surface::{Geometry, SurfaceApi};
use embedded_gpui::{
    Payload, Plugin, Ref, Remote, decode, encode, open_view, register_plugin, root, share,
    share_root, share_with, shared,
};
use embedded_gpui_util::Revocable;
use gpui::{
    App, Context, Entity, EventEmitter, MouseDownEvent, Task, WeakEntity, Window, div, prelude::*,
    rgb,
};
use test_schema::{
    ChameleonApi, ChameleonState, CounterMilestone, FactoryApi, GatekeeperApi, ItemApi, ItemInfo,
    TestCounterApi, TestHost, TestHostCaller as _, TestPlugin, VaultApi, ViewProbeApi,
};

/// The plugin's whole bootstrap: construct the root object and install it at this end's
/// id 0. Everything else is reached through the root's methods.
struct TestGuest {
    _root: Entity<Root>,
}

impl Plugin for TestGuest {
    fn new(cx: &mut App) -> Self {
        let host = root::<TestHost>();
        let root = cx.new(|_| Root {
            host,
            counter: None,
            factory: None,
            gatekeeper: None,
            chameleon: None,
            probes: Vec::new(),
        });
        share_root(&root, cx);
        TestGuest { _root: root }
    }
}

register_plugin!(TestGuest);

/// The root object: each method is a lazy factory, creating its entity on first call
/// and returning the same ref thereafter. The root keeps the entities alive itself so a
/// release (last remote dropped on the other end) does not invalidate the cached ref.
struct Root {
    host: Remote<TestHost>,
    counter: Option<(Entity<Counter>, Ref<TestCounterApi>)>,
    factory: Option<(Entity<Factory>, Ref<FactoryApi>)>,
    gatekeeper: Option<(Entity<Gatekeeper>, Ref<GatekeeperApi>)>,
    chameleon: Option<(Entity<Chameleon>, Ref<ChameleonApi>)>,
    /// Probes for every view mounted so far; the root owns them so their refs stay valid.
    probes: Vec<Entity<ViewProbe>>,
}

#[shared]
impl TestPlugin for Root {
    fn counter(&mut self, cx: &mut Context<Self>) -> Ref<TestCounterApi> {
        if let Some((_, reference)) = &self.counter {
            return reference.clone();
        }
        let counter = cx.new(|_| Counter { count: 0 });
        let reference = share(&counter, cx);
        self.counter = Some((counter, reference.clone()));
        reference
    }

    fn factory(&mut self, cx: &mut Context<Self>) -> Ref<FactoryApi> {
        if let Some((_, reference)) = &self.factory {
            return reference.clone();
        }
        let factory = cx.new(|_| Factory { created: 0 });
        let reference = share(&factory, cx);
        self.factory = Some((factory, reference.clone()));
        reference
    }

    fn gatekeeper(&mut self, cx: &mut Context<Self>) -> Ref<GatekeeperApi> {
        if let Some((_, reference)) = &self.gatekeeper {
            return reference.clone();
        }
        let gatekeeper = cx.new(|_| Gatekeeper { guarded: 0 });
        let reference = share(&gatekeeper, cx);
        self.gatekeeper = Some((gatekeeper, reference.clone()));
        reference
    }

    fn chameleon(&mut self, cx: &mut Context<Self>) -> Ref<ChameleonApi> {
        if let Some((_, reference)) = &self.chameleon {
            return reference.clone();
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
        self.chameleon = Some((chameleon, reference.clone()));
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

    fn mount(&mut self, surface: Ref<SurfaceApi>, cx: &mut Context<Self>) -> Ref<ViewProbeApi> {
        let probe = cx.new(|_| ViewProbe { view: None });
        let weak_probe = probe.downgrade();
        let opened = open_view(surface, cx, |_, cx| {
            let view = cx.new(|_| ProbeView {
                geometry: None,
                clicks: 0,
            });
            weak_probe
                .update(cx, |probe, _| probe.view = Some(view.downgrade()))
                .ok();
            view
        });
        if let Err(error) = opened {
            embedded_gpui::log::error!("test_plugin: open_view failed: {error:#}");
        }
        let reference = share(&probe, cx);
        self.probes.push(probe);
        reference
    }
}

/// The root view of a mounted window: records what the host drives it with.
struct ProbeView {
    geometry: Option<Geometry>,
    clicks: u32,
}

impl Render for ProbeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = window.viewport_size();
        self.geometry = Some(Geometry {
            width: f32::from(size.width),
            height: f32::from(size.height),
            scale_factor: window.scale_factor(),
        });
        div().size_full().bg(rgb(0x336699)).on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.clicks += 1;
                cx.notify();
            }),
        )
    }
}

/// Plugin-homed and root-owned, so it outlives the window it reports on.
struct ViewProbe {
    view: Option<WeakEntity<ProbeView>>,
}

#[shared]
impl ViewProbeApi for ViewProbe {
    fn last_geometry(&mut self, cx: &mut Context<Self>) -> Option<Geometry> {
        let view = self.view.as_ref()?.upgrade()?;
        view.read(cx).geometry
    }

    fn clicks(&mut self, cx: &mut Context<Self>) -> u32 {
        self.view
            .as_ref()
            .and_then(|view| view.upgrade())
            .map(|view| view.read(cx).clicks)
            .unwrap_or(0)
    }

    fn view_alive(&mut self, _cx: &mut Context<Self>) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.upgrade().is_some())
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
    fn create(&mut self, label: String, cx: &mut Context<Self>) -> Ref<ItemApi> {
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
    fn guard(&mut self, vault: Ref<VaultApi>, cx: &mut Context<Self>) -> Ref<VaultApi> {
        self.guarded += 1;
        cx.notify();
        // The membrane is the stock caretaker from embedded_gpui_util: every method
        // forwards to the wrapped vault capability, and revoking drops the inner remote
        // (auto-release cascades to the vault's home).
        let vault = vault.connect();
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
        target: Ref<ItemApi>,
        method: String,
        payload: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Vec<u8>>> {
        let remote = target.connect();
        let receipt = remote.call_raw(&method, Payload::from_parts(payload, Vec::new()), cx);
        cx.spawn(async move |_, _| {
            let outcome = receipt.await;
            drop(remote);
            outcome.map(|payload| payload.bytes)
        })
    }
}

struct Chameleon {
    mode: String,
    pokes: u32,
}
