//! Common object-capability patterns built on `embedded_gpui`.
//!
//! Everything here is side-agnostic: each wrapper holds a [`Remote`], and remotes look
//! the same on both ends of the boundary. All three forwarders implement
//! [`Shared`](embedded_gpui::Shared), so sharing one is exactly like sharing any other
//! entity: `share(&wrapper, cx)`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::anyhow;
use embedded_gpui::{
    EventSink, Interface, Message, Methods, Opaque, Payload, Registry, Remote, Shared,
    WILDCARD_METHOD,
};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};

/// A revocable membrane (OCAP "caretaker") around any capability you hold.
///
/// Wrap a remote in a `Revocable`, share the *wrapper*, and hand out the resulting ref
/// instead of the original. To the recipient it is indistinguishable from the real
/// entity: notifies and events pass through, and every method — including ones this
/// code has never heard of — forwards asynchronously to the wrapped capability.
///
/// The wrapper is *transitive*: every ref that crosses it, in either direction (call
/// arguments, responses, event payloads), is itself wrapped in a `Revocable` sharing the
/// same revocation switch, and refs to those wrappers coming back are unwrapped so the
/// wrapped object sees its own objects. The ref table on every payload is what makes
/// this possible without parsing anything. When you call [`Revocable::revoke`], every
/// wrapper drops its inner remote (auto-release cascades to the homes) and every
/// subsequent call through any of them fails with `"capability revoked"`.
///
/// Refs homed on the wrapper's own end pass through unwrapped (connecting to a local
/// home is not supported); wrap capabilities you *hold*, not ones you *are*.
///
/// Revocation authority stays with whoever holds this entity; it is deliberately not
/// exposed over the wire. To let a *peer* revoke (or to add any other control surface),
/// share with `share_with` and register extra methods alongside
/// [`Revocable::register`]:
///
/// ```ignore
/// let revocable = Revocable::new(vault, cx);
/// let guarded_ref = share_with(
///     &revocable,
///     |methods| {
///         Revocable::register(methods);
///         methods.on("revoke", |entity, _, _, cx| {
///             entity.update(cx, |revocable, cx| revocable.revoke(cx));
///             encode(&())
///         });
///     },
///     cx,
/// );
/// ```
pub struct Revocable<S: Interface> {
    target: Option<Remote<S>>,
    membrane: Membrane,
    _notify: Option<Subscription>,
}

/// The revocation switch and wrapper table shared by every `Revocable` a membrane has
/// minted: the root wrapper plus one per ref that ever crossed it.
#[derive(Clone, Default)]
struct Membrane(Rc<RefCell<MembraneState>>);

#[derive(Default)]
struct MembraneState {
    revoked: bool,
    /// Inner object id -> the wrapper minted for it and the wrapper's own object id.
    wrappers: HashMap<u64, (Entity<Revocable<Opaque>>, u64)>,
    /// Wrapper object id -> inner object id, to unwrap refs that come back.
    inner_of: HashMap<u64, u64>,
}

impl Membrane {
    /// Rewrite a payload's ref table for the other side of the membrane: wrappers are
    /// unwrapped, everything else is wrapped (minting on first sight). Bytes are copied
    /// verbatim — they name refs by index, and the indices do not change.
    fn translate(&self, payload: &Payload, registry: &Registry, cx: &mut App) -> Payload {
        let refs = payload
            .refs
            .iter()
            .map(|&id| self.translate_ref(id, registry, cx))
            .collect();
        Payload::from_parts(payload.bytes.clone(), refs)
    }

    fn translate_ref(&self, id: u64, registry: &Registry, cx: &mut App) -> u64 {
        {
            let state = self.0.borrow();
            if let Some(&inner) = state.inner_of.get(&id) {
                return inner;
            }
            if let Some((_, wrapper)) = state.wrappers.get(&id) {
                return *wrapper;
            }
        }
        if registry.is_local(id) {
            log::warn!(
                "embedded_gpui_util: ref {id} is homed on this end; passing through the \
                 membrane unwrapped"
            );
            return id;
        }
        let wrapper = Revocable::new_in(self.clone(), registry.connect::<Opaque>(id), cx);
        let wrapper_ref = registry.share(&wrapper, cx);
        let wrapper_id = wrapper_ref.entity_id();
        let mut state = self.0.borrow_mut();
        state.wrappers.insert(id, (wrapper, wrapper_id));
        state.inner_of.insert(wrapper_id, id);
        wrapper_id
    }

    /// Flip the switch: sever every wrapper minted so far. Returns them so the caller
    /// can sever them outside this borrow.
    fn revoke(&self) -> Vec<Entity<Revocable<Opaque>>> {
        let mut state = self.0.borrow_mut();
        state.revoked = true;
        state.inner_of.clear();
        state
            .wrappers
            .drain()
            .map(|(_, (wrapper, _))| wrapper)
            .collect()
    }
}

impl<S: Interface> Revocable<S> {
    /// Wrap `target` as the root of a new membrane. The wrapped capability's notifies
    /// republish through the wrapper.
    pub fn new(target: Remote<S>, cx: &mut App) -> Entity<Self> {
        Self::new_in(Membrane::default(), target, cx)
    }

    fn new_in(membrane: Membrane, target: Remote<S>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let this = cx.weak_entity();
            let notify = target.observe(cx, move |cx| {
                this.update(cx, |_, cx| cx.notify()).ok();
            });
            Self {
                target: Some(target),
                membrane,
                _notify: Some(notify),
            }
        })
    }

    /// Sever the whole membrane: this wrapper and every wrapper it minted drop their
    /// inner remotes — if one was the last handle, auto-release tells its home to let
    /// the entity go — and every further call through any of them fails.
    pub fn revoke(&mut self, cx: &mut Context<Self>) {
        self.sever(cx);
        for wrapper in self.membrane.revoke() {
            wrapper.update(cx, |wrapper, cx| wrapper.sever(cx));
        }
    }

    fn sever(&mut self, cx: &mut Context<Self>) {
        self.target = None;
        self._notify = None;
        cx.notify();
    }

    /// Whether [`Revocable::revoke`] has run.
    pub fn is_revoked(&self) -> bool {
        self.target.is_none()
    }

    /// Install the forwarding handler: a wildcard that pipes every method through to the
    /// wrapped capability, translating the refs in both the request and the response
    /// through the membrane, and resolving when the real response comes back.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_async(WILDCARD_METHOD, |entity, method, payload, cx| {
            let (target, membrane) = {
                let this = entity.read(cx);
                (this.target.clone(), this.membrane.clone())
            };
            let Some(target) = target else {
                return Task::ready(Err(anyhow!("capability revoked")));
            };
            let registry = target.registry();
            let payload = membrane.translate(payload, &registry, cx);
            let receipt = target.call_raw(method, payload, cx);
            cx.spawn(async move |cx| {
                let response = receipt.await?;
                cx.update(|cx| {
                    if membrane.0.borrow().revoked {
                        return Err(anyhow!("capability revoked"));
                    }
                    Ok(membrane.translate(&response, &registry, cx))
                })
            })
        });
    }
}

impl<S: Interface> Shared<S> for Revocable<S> {
    fn methods(methods: &mut Methods<S, Self>) {
        Self::register(methods);
    }

    fn events(entity: &Entity<Self>, sink: EventSink, cx: &mut App) -> Vec<Subscription> {
        let (target, membrane) = {
            let this = entity.read(cx);
            (this.target.clone(), this.membrane.clone())
        };
        let Some(target) = target else {
            return Vec::new();
        };
        let registry = target.registry();
        let wrapper = entity.downgrade();
        vec![target.subscribe_raw(cx, move |name, payload, cx| {
            let live = wrapper
                .read_with(cx, |wrapper, _| wrapper.target.is_some())
                .unwrap_or(false);
            if live {
                sink(name, membrane.translate(payload, &registry, cx), cx);
            }
        })]
    }
}

/// An allowlist forwarder: the userland form of attenuation. Wrap a capability you
/// hold, list the methods that may pass, share the wrapper, and hand out *its* ref.
/// Everything else is rejected without ever reaching the wrapped entity — monotonic by
/// construction, since a wrapper can only forward what it can itself call, and no
/// cooperation from the entity's author is required.
///
/// Attenuation is method-level and single-hop: notifies, events, and any refs in
/// payloads pass through unfiltered (refs a method returns carry their own, full
/// authority; wrap them too if that matters). Compose with [`Revocable`] for a
/// transitive membrane.
///
/// ```ignore
/// let readonly = Attenuated::new(item_remote, &["describe"], cx);
/// let readonly_ref = share(&readonly, cx);
/// ```
pub struct Attenuated<S: Interface> {
    target: Remote<S>,
    allowed: Vec<String>,
    _notify: Subscription,
}

impl<S: Interface> Attenuated<S> {
    /// Wrap `target`, permitting only the listed methods through.
    pub fn new(target: Remote<S>, allowed: &[&str], cx: &mut App) -> Entity<Self> {
        let allowed = allowed.iter().map(|method| method.to_string()).collect();
        cx.new(|cx| {
            let this = cx.weak_entity();
            let notify = target.observe(cx, move |cx| {
                this.update(cx, |_, cx| cx.notify()).ok();
            });
            Self {
                target,
                allowed,
                _notify: notify,
            }
        })
    }

    /// Install the filtering forwarder: allowed methods pipe through byte-for-byte,
    /// everything else fails without touching the wrapped capability.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_async(WILDCARD_METHOD, |entity, method, payload, cx| {
            let permitted = entity
                .read(cx)
                .allowed
                .iter()
                .any(|allowed| allowed == method);
            if !permitted {
                return Task::ready(Err(anyhow!(
                    "method {method:?} is not permitted by this capability"
                )));
            }
            let target = entity.read(cx).target.clone();
            let receipt = target.call_raw(method, payload.clone(), cx);
            cx.spawn(async move |_| receipt.await)
        });
    }
}

impl<S: Interface> Shared<S> for Attenuated<S> {
    fn methods(methods: &mut Methods<S, Self>) {
        Self::register(methods);
    }

    fn events(entity: &Entity<Self>, sink: EventSink, cx: &mut App) -> Vec<Subscription> {
        let target = entity.read(cx).target.clone();
        vec![target.subscribe_raw(cx, move |name, payload, cx| {
            sink(name, payload.clone(), cx);
        })]
    }
}

/// One forwarded call, as remembered by an [`Audited`] wrapper.
#[derive(Clone, Debug)]
pub struct AuditRecord {
    pub method: String,
    pub payload_len: usize,
    /// The object ids the call's payload carried: every capability that passed through.
    pub refs: Vec<u64>,
    /// `None` while the forwarded call is still in flight.
    pub completed: Option<bool>,
}

/// An accounting forwarder: forwards every method like a transparent caretaker, but
/// remembers each call — method name, payload size, the refs it carried, and eventually
/// whether it succeeded — and logs it. Observe the entity (`cx.observe`) to react to
/// new records; read [`Audited::records`] to inspect them.
///
/// Reading the ledger is itself an authority: it stays with whoever holds this entity.
/// Exposing it over the wire (or to a UI) is an explicit choice, exactly like
/// revocation on [`Revocable`].
pub struct Audited<S: Interface> {
    target: Remote<S>,
    records: Vec<AuditRecord>,
    _notify: Subscription,
}

impl<S: Interface> Audited<S> {
    /// Wrap `target`; every call forwarded through the wrapper is recorded.
    pub fn new(target: Remote<S>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let this = cx.weak_entity();
            let notify = target.observe(cx, move |cx| {
                this.update(cx, |_, cx| cx.notify()).ok();
            });
            Self {
                target,
                records: Vec::new(),
                _notify: notify,
            }
        })
    }

    /// The calls forwarded so far, oldest first.
    pub fn records(&self) -> &[AuditRecord] {
        &self.records
    }

    /// Install the recording forwarder.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_async(WILDCARD_METHOD, |entity, method, payload, cx| {
            let index = entity.update(cx, |audited, cx| {
                log::info!(
                    "audited capability: {:?} called with {} bytes and {} refs",
                    method,
                    payload.bytes.len(),
                    payload.refs.len()
                );
                audited.records.push(AuditRecord {
                    method: method.to_string(),
                    payload_len: payload.bytes.len(),
                    refs: payload.refs.clone(),
                    completed: None,
                });
                cx.notify();
                audited.records.len() - 1
            });
            let target = entity.read(cx).target.clone();
            let receipt = target.call_raw(method, payload.clone(), cx);
            let entity = entity.downgrade();
            cx.spawn(async move |cx| {
                let outcome = receipt.await;
                entity
                    .update(cx, |audited, cx| {
                        if let Some(record) = audited.records.get_mut(index) {
                            record.completed = Some(outcome.is_ok());
                        }
                        cx.notify();
                    })
                    .ok();
                outcome
            })
        });
    }
}

impl<S: Interface> Shared<S> for Audited<S> {
    fn methods(methods: &mut Methods<S, Self>) {
        Self::register(methods);
    }

    fn events(entity: &Entity<Self>, sink: EventSink, cx: &mut App) -> Vec<Subscription> {
        let target = entity.read(cx).target.clone();
        vec![target.subscribe_raw(cx, move |name, payload, cx| {
            sink(name, payload.clone(), cx);
        })]
    }
}

/// A local, observable cache of remote state: snapshots as a *library* instead of a
/// protocol feature.
///
/// Reads on a [`Remote`] are calls, but GPUI rendering is synchronous — so anything
/// that renders remote state natively wants a local copy that notifies when it changes.
/// `Mirror` is that copy: it observes the remote, refetches through the given message
/// whenever the home notifies (coalescing bursts into one in-flight call), and holds
/// the latest value in an ordinary observable entity.
///
/// ```ignore
/// let commands: Entity<Mirror<Vec<PaletteEntry>>> = Mirror::new(palette, Commands {}, cx);
/// cx.observe(&commands, |_, _, cx| cx.notify()).detach();
/// // in render:
/// let entries = commands.read(cx).latest().cloned().unwrap_or_default();
/// ```
pub struct Mirror<T: 'static> {
    latest: Option<T>,
    fetching: bool,
    dirty: bool,
    _observation: Subscription,
}

impl<T: 'static> Mirror<T> {
    /// Mirror the value of `request` (a call returning `T`) on `remote`, refreshing on
    /// every notify from the home. The home notifies once on subscription, so the first
    /// value arrives without any explicit kick.
    pub fn new<S, M>(remote: Remote<S>, request: M, cx: &mut App) -> Entity<Self>
    where
        S: Interface,
        M: Message<Spec = S, Response = T> + Clone,
    {
        let mirror = cx.new(|cx| {
            let this = cx.weak_entity();
            let fetch_remote = remote.clone();
            let fetch_request = request.clone();
            let observation = remote.observe(cx, move |cx| {
                Self::refresh(this.clone(), &fetch_remote, &fetch_request, cx);
            });
            Self {
                latest: None,
                fetching: false,
                dirty: false,
                _observation: observation,
            }
        });
        // The subscription's initial notify may predate this mirror; fetch once
        // unconditionally.
        Self::refresh(mirror.downgrade(), &remote, &request, cx);
        mirror
    }

    /// The most recently fetched value, if any has arrived yet.
    pub fn latest(&self) -> Option<&T> {
        self.latest.as_ref()
    }

    fn refresh<S, M>(this: WeakEntity<Self>, remote: &Remote<S>, request: &M, cx: &mut App)
    where
        S: Interface,
        M: Message<Spec = S, Response = T> + Clone,
    {
        let started = this
            .update(cx, |mirror, _| {
                if mirror.fetching {
                    // A fetch is already in flight; remember to go again with the newer
                    // state. Any number of notifies coalesce into one trailing fetch.
                    mirror.dirty = true;
                    false
                } else {
                    mirror.fetching = true;
                    true
                }
            })
            .unwrap_or(false);
        if !started {
            return;
        }
        let receipt = remote.call(request.clone(), cx);
        let remote = remote.clone();
        let request = request.clone();
        cx.spawn(async move |cx| {
            let outcome = receipt.await;
            cx.update(move |cx| {
                let redo = this
                    .update(cx, |mirror, cx| {
                        mirror.fetching = false;
                        match outcome {
                            Ok(value) => {
                                mirror.latest = Some(value);
                                cx.notify();
                            }
                            Err(error) => {
                                log::warn!("embedded_gpui_util: mirror refresh failed: {error:#}")
                            }
                        }
                        std::mem::take(&mut mirror.dirty)
                    })
                    .unwrap_or(false);
                if redo {
                    Self::refresh(this, &remote, &request, cx);
                }
            });
        })
        .detach();
    }
}
