//! Common object-capability patterns built on `embedded_gpui`.
//!
//! Everything here is side-agnostic: each wrapper holds a [`Remote`], and remotes look
//! the same on both ends of the boundary. All three forwarders implement
//! [`Shared`](embedded_gpui::Shared), so sharing one is exactly like sharing any other
//! entity: `share(&wrapper, cx)`.

use anyhow::anyhow;
use embedded_gpui::{EventSink, Interface, Message, Methods, Remote, Shared, WILDCARD_METHOD};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};

/// A revocable forwarder (OCAP "caretaker") around any capability you hold.
///
/// Wrap a remote in a `Revocable`, share the *wrapper*, and hand out the resulting ref
/// instead of the original. To the recipient it is indistinguishable from the real
/// entity: notifies and events pass through, and every method — including ones this
/// code has never heard of — forwards asynchronously to the wrapped capability. When
/// you call [`Revocable::revoke`], the wrapper drops the inner remote (auto-release
/// cascades to its home) and every subsequent call fails with `"capability revoked"`.
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
    _notify: Option<Subscription>,
}

impl<S: Interface> Revocable<S> {
    /// Wrap `target`. The wrapped capability's notifies republish through the wrapper.
    pub fn new(target: Remote<S>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let this = cx.weak_entity();
            let notify = target.observe(cx, move |cx| {
                this.update(cx, |_, cx| cx.notify()).ok();
            });
            Self {
                target: Some(target),
                _notify: Some(notify),
            }
        })
    }

    /// Sever the wrapper from the wrapped capability. The inner remote drops here — if
    /// it was the last handle, auto-release tells its home to let the entity go — and
    /// every further call through the wrapper fails.
    pub fn revoke(&mut self, cx: &mut Context<Self>) {
        self.target = None;
        self._notify = None;
        cx.notify();
    }

    /// Whether [`Revocable::revoke`] has run.
    pub fn is_revoked(&self) -> bool {
        self.target.is_none()
    }

    /// Install the forwarding handler: a wildcard that pipes every method through to the
    /// wrapped capability, byte-for-byte, resolving when the real response comes back.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_async(WILDCARD_METHOD, |entity, method, payload, cx| match entity
            .read(cx)
            .target
            .clone()
        {
            Some(target) => {
                let receipt = target.call_raw(method, payload.to_vec(), cx);
                cx.spawn(async move |_| receipt.await)
            }
            None => Task::ready(Err(anyhow!("capability revoked"))),
        });
    }
}

impl<S: Interface> Shared<S> for Revocable<S> {
    fn methods(methods: &mut Methods<S, Self>) {
        Self::register(methods);
    }

    fn events(entity: &Entity<Self>, sink: EventSink, cx: &mut App) -> Vec<Subscription> {
        let Some(target) = entity.read(cx).target.clone() else {
            return Vec::new();
        };
        let wrapper = entity.downgrade();
        vec![target.subscribe_raw(cx, move |name, payload, cx| {
            let live = wrapper
                .read_with(cx, |wrapper, _| wrapper.target.is_some())
                .unwrap_or(false);
            if live {
                sink(name, payload.to_vec(), cx);
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
/// Attenuation is method-level: notifies and events pass through unfiltered (they flow
/// outward, revealing only what the entity already chose to broadcast).
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
            let receipt = target.call_raw(method, payload.to_vec(), cx);
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
            sink(name, payload.to_vec(), cx);
        })]
    }
}

/// One forwarded call, as remembered by an [`Audited`] wrapper.
#[derive(Clone, Debug)]
pub struct AuditRecord {
    pub method: String,
    pub payload_len: usize,
    /// `None` while the forwarded call is still in flight.
    pub completed: Option<bool>,
}

/// An accounting forwarder: forwards every method like a transparent caretaker, but
/// remembers each call — method name, payload size, and eventually whether it
/// succeeded — and logs it. Observe the entity (`cx.observe`) to react to new records;
/// read [`Audited::records`] to inspect them.
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
                    "audited capability: {:?} called with {} bytes",
                    method,
                    payload.len()
                );
                audited.records.push(AuditRecord {
                    method: method.to_string(),
                    payload_len: payload.len(),
                    completed: None,
                });
                cx.notify();
                audited.records.len() - 1
            });
            let target = entity.read(cx).target.clone();
            let receipt = target.call_raw(method, payload.to_vec(), cx);
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
            sink(name, payload.to_vec(), cx);
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
