//! The plugin half of `tests/shared_entities.rs`: one root object through which the
//! host reaches a zoo of entities exercising calls, events, capability refs,
//! attenuation, and fully dynamic dispatch.

use anyhow::anyhow;
use embedded_gpui::surface::SurfaceApi;
use embedded_gpui::{
    Payload, Plugin, Ref, Remote, decode, encode, open_view, register_plugin, root, share,
    share_root, share_with, shared, use_clipboard,
};
use embedded_gpui_util::Revocable;
use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, KeyDownEvent, MouseDownEvent, Pixels, Task,
    UTF16Selection, WeakEntity, Window, canvas, div, prelude::*, rgb,
};
use std::ops::Range;
use test_schema::{
    ChameleonApi, ChameleonState, CounterMilestone, FactoryApi, GatekeeperApi, ItemApi, ItemInfo,
    SeenGeometry, TestCounterApi, TestHost, TestHostCaller as _, TestPlugin, VaultApi,
    ViewProbeApi,
};

/// The plugin's whole bootstrap: construct the root object and install it at this end's
/// id 0. Everything else is reached through the root's methods.
struct TestGuest {
    _root: Entity<Root>,
}

impl Plugin for TestGuest {
    fn new(cx: &mut App) -> Self {
        let host = root::<TestHost>();
        // The clipboard is a capability the host may or may not hand out; ask, and if it
        // arrives, let the platform's synchronous clipboard read from it.
        let clipboard = host.clipboard(cx);
        cx.spawn(async move |cx| {
            if let Ok(clipboard) = clipboard.await {
                cx.update(|cx| use_clipboard(clipboard, cx));
            }
        })
        .detach();
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

    fn spin(&mut self, millis: u64, _cx: &mut Context<Self>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
        while std::time::Instant::now() < deadline {
            std::hint::spin_loop();
        }
    }

    fn mount(&mut self, surface: Ref<SurfaceApi>, cx: &mut Context<Self>) -> Ref<ViewProbeApi> {
        let probe = cx.new(|_| ViewProbe { view: None });
        let weak_probe = probe.downgrade();
        let view = cx.new(|cx| ProbeView {
            geometry: None,
            clicks: 0,
            handled_keys: 0,
            text: String::new(),
            selection: 0..0,
            marked: None,
            focus_handle: cx.focus_handle().tab_stop(true),
        });
        weak_probe
            .update(cx, |probe, _| probe.view = Some(view.downgrade()))
            .ok();
        if let Err(error) = open_view(surface, view, cx) {
            embedded_gpui::log::error!("test_plugin: open_view failed: {error:#}");
        }
        let reference = share(&probe, cx);
        self.probes.push(probe);
        reference
    }
}

/// The root view of a mounted window: records what the host drives it with.
struct ProbeView {
    geometry: Option<SeenGeometry>,
    clicks: u32,
    handled_keys: u32,
    /// A minimal text field, so the host's synchronous input queries have something to
    /// talk to.
    text: String,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    focus_handle: FocusHandle,
}

impl EntityInputHandler for ProbeView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let chars: Vec<u16> = self.text.encode_utf16().collect();
        let end = range.end.min(chars.len());
        String::from_utf16(&chars[range.start.min(end)..end]).ok()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or(self.selection.clone());
        let mut chars: Vec<u16> = self.text.encode_utf16().collect();
        let end = range.end.min(chars.len());
        let start = range.start.min(end);
        chars.splice(start..end, text.encode_utf16());
        self.text = String::from_utf16_lossy(&chars);
        let caret = start + text.encode_utf16().count();
        self.selection = caret..caret;
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = range
            .clone()
            .or_else(|| self.marked.clone())
            .unwrap_or(self.selection.clone())
            .start;
        self.replace_text_in_range(range, new_text, window, cx);
        self.marked = Some(start..start + new_text.encode_utf16().count());
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.text.encode_utf16().count())
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = range_utf16;
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.text.encode_utf16().count())
    }
}

impl Render for ProbeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A view is a root of the plugin's window, not a window: its slot's geometry is
        // the bounds it is laid out in, measured here at prepaint.
        let measured = cx.entity();
        let handler = cx.entity();
        let focus_handle = self.focus_handle.clone();
        div()
            .size_full()
            .bg(rgb(0x336699))
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.clicks += 1;
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.key == "enter" {
                    this.handled_keys += 1;
                    cx.stop_propagation();
                    cx.notify();
                } else if keystroke.modifiers.platform && keystroke.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        this.replace_text_in_range(None, &text, window, cx);
                    }
                    cx.stop_propagation();
                } else if keystroke.modifiers.platform && keystroke.key == "c" {
                    cx.write_to_clipboard(ClipboardItem::new_string(this.text.clone()));
                    cx.stop_propagation();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let viewport = window.viewport_size();
                        let geometry = SeenGeometry {
                            x: f32::from(bounds.origin.x),
                            y: f32::from(bounds.origin.y),
                            width: f32::from(bounds.size.width),
                            height: f32::from(bounds.size.height),
                            viewport_width: f32::from(viewport.width),
                            viewport_height: f32::from(viewport.height),
                            scale_factor: window.scale_factor(),
                            active: window.is_window_active(),
                        };
                        measured.update(cx, |this, _| this.geometry = Some(geometry));
                    },
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, handler.clone()),
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}

/// Plugin-homed and root-owned, so it outlives the window it reports on.
struct ViewProbe {
    view: Option<WeakEntity<ProbeView>>,
}

#[shared]
impl ViewProbeApi for ViewProbe {
    fn last_geometry(&mut self, cx: &mut Context<Self>) -> Option<SeenGeometry> {
        let view = self.view.as_ref()?.upgrade()?;
        view.read(cx).geometry
    }

    fn focus(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_ref().and_then(|view| view.upgrade()) else {
            return;
        };
        let focus_handle = view.read(cx).focus_handle.clone();
        // A view is a root of the guest window mirroring its host window; the plugin
        // reaches that window like any other.
        for window in cx.windows() {
            window
                .update(cx, |_, window, cx| window.focus(&focus_handle, cx))
                .ok();
        }
    }

    fn text(&mut self, cx: &mut Context<Self>) -> String {
        self.view
            .as_ref()
            .and_then(|view| view.upgrade())
            .map(|view| view.read(cx).text.clone())
            .unwrap_or_default()
    }

    fn handled_keys(&mut self, cx: &mut Context<Self>) -> u32 {
        self.view
            .as_ref()
            .and_then(|view| view.upgrade())
            .map(|view| view.read(cx).handled_keys)
            .unwrap_or(0)
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
