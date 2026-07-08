//! Common object-capability patterns built on `embedded_gpui`.
//!
//! Everything here is side-agnostic: the same code runs in the guest (wrapping a
//! `Remote<S>`) and on the host (wrapping a `HostRemote<S>`), because it only relies on
//! the [`SharedCaller`] surface.

use anyhow::anyhow;
use embedded_gpui::{Methods, SharedCaller, SharedEntitySource, SharedSpec, WILDCARD_METHOD};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task};

/// A revocable forwarder (OCAP "caretaker") around any capability you hold.
///
/// Wrap a remote in a `Revocable`, share the *wrapper*, and hand out the resulting ref
/// instead of the original. To the recipient it is indistinguishable from the real
/// entity: snapshots pass through, and every method — including ones this code has
/// never heard of — forwards asynchronously to the wrapped capability. When you call
/// [`Revocable::revoke`], the wrapper drops the inner remote (auto-release cascades to
/// its home), the last snapshot freezes, and every subsequent call fails with
/// `"capability revoked"`.
///
/// Revocation authority stays with whoever holds this entity; it is deliberately not
/// exposed over the wire. To let a *peer* revoke (or to add any other control surface),
/// register extra methods alongside [`Revocable::register`]:
///
/// ```ignore
/// let revocable = Revocable::new(vault, placeholder_snapshot, cx);
/// let guarded_ref = share_anonymous::<VaultSpec, _>(
///     &revocable,
///     |methods| {
///         Revocable::register(methods);
///         methods.on_raw("revoke", |entity, _, _, cx| {
///             entity.update(cx, |revocable, cx| revocable.revoke(cx));
///             encode(&())
///         });
///     },
///     cx,
/// );
/// ```
pub struct Revocable<S: SharedSpec, C: SharedCaller<S>> {
    target: Option<C>,
    /// Served while the target replica is empty, and frozen from the target's last
    /// state at revocation.
    fallback: S::Snapshot,
    _observation: Option<Subscription>,
}

impl<S, C> Revocable<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    /// Wrap `target`. `placeholder` is served to subscribers until the target's first
    /// snapshot arrives. Changes to the target's replica republish through the wrapper.
    pub fn new(target: C, placeholder: S::Snapshot, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let observation = cx.observe(target.shared_replica(), |_, _, cx| cx.notify());
            Self {
                target: Some(target),
                fallback: placeholder,
                _observation: Some(observation),
            }
        })
    }

    /// Sever the wrapper from the wrapped capability. The inner remote drops here — if
    /// it was the last handle, auto-release tells its home to let the entity go — and
    /// the wrapper freezes the last observed snapshot for any remaining subscribers.
    pub fn revoke(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.target.take()
            && let Some(state) = target.shared_replica().read(cx).state.clone()
        {
            self.fallback = state;
        }
        self._observation = None;
        cx.notify();
    }

    /// Whether [`Revocable::revoke`] has run.
    pub fn is_revoked(&self) -> bool {
        self.target.is_none()
    }

    /// Install the forwarding handler: a wildcard that pipes every method through to the
    /// wrapped capability, byte-for-byte, resolving when the real response comes back.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_raw_async(WILDCARD_METHOD, |entity, method, payload, cx| match entity
            .read(cx)
            .target
            .clone()
        {
            Some(target) => {
                let receipt = target.forward_shared(method, payload.to_vec(), cx);
                cx.spawn(async move |_| receipt.await)
            }
            None => Task::ready(Err(anyhow!("capability revoked"))),
        });
    }
}

impl<S, C> SharedEntitySource<S> for Revocable<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    fn snapshot(&self, cx: &App) -> S::Snapshot {
        self.target
            .as_ref()
            .and_then(|target| target.shared_replica().read(cx).state.clone())
            .unwrap_or_else(|| self.fallback.clone())
    }
}

/// An allowlist forwarder: the userland form of attenuation. Wrap a capability you
/// hold, list the methods that may pass, share the wrapper, and hand out *its* ref.
/// Everything else is rejected without ever reaching the wrapped entity — monotonic by
/// construction, since a wrapper can only forward what it can itself call, and no
/// cooperation from the entity's author is required.
///
/// ```ignore
/// let readonly = Attenuated::new(item_remote, &[], placeholder_snapshot, cx);
/// let readonly_ref = share_anonymous::<ItemSpec, _>(&readonly, Attenuated::register, cx);
/// ```
pub struct Attenuated<S: SharedSpec, C: SharedCaller<S>> {
    target: C,
    allowed: Vec<String>,
    /// Served while the target replica is empty.
    fallback: S::Snapshot,
    _observation: Subscription,
}

impl<S, C> Attenuated<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    /// Wrap `target`, permitting only the listed methods through.
    pub fn new(
        target: C,
        allowed: &[&str],
        placeholder: S::Snapshot,
        cx: &mut App,
    ) -> Entity<Self> {
        let allowed = allowed.iter().map(|method| method.to_string()).collect();
        cx.new(|cx| {
            let observation = cx.observe(target.shared_replica(), |_, _, cx| cx.notify());
            Self {
                target,
                allowed,
                fallback: placeholder,
                _observation: observation,
            }
        })
    }

    /// Install the filtering forwarder: allowed methods pipe through byte-for-byte,
    /// everything else fails without touching the wrapped capability.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_raw_async(WILDCARD_METHOD, |entity, method, payload, cx| {
            let target = entity
                .read(cx)
                .allowed
                .iter()
                .any(|allowed| allowed == method);
            if !target {
                return Task::ready(Err(anyhow!(
                    "method {method:?} is not permitted by this capability"
                )));
            }
            let target = entity.read(cx).target.clone();
            let receipt = target.forward_shared(method, payload.to_vec(), cx);
            cx.spawn(async move |_| receipt.await)
        });
    }
}

impl<S, C> SharedEntitySource<S> for Attenuated<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    fn snapshot(&self, cx: &App) -> S::Snapshot {
        self.target
            .shared_replica()
            .read(cx)
            .state
            .clone()
            .unwrap_or_else(|| self.fallback.clone())
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
pub struct Audited<S: SharedSpec, C: SharedCaller<S>> {
    target: C,
    records: Vec<AuditRecord>,
    /// Served while the target replica is empty.
    fallback: S::Snapshot,
    _observation: Subscription,
}

impl<S, C> Audited<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    /// Wrap `target`; every call forwarded through the wrapper is recorded.
    pub fn new(target: C, placeholder: S::Snapshot, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let observation = cx.observe(target.shared_replica(), |_, _, cx| cx.notify());
            Self {
                target,
                records: Vec::new(),
                fallback: placeholder,
                _observation: observation,
            }
        })
    }

    /// The calls forwarded so far, oldest first.
    pub fn records(&self) -> &[AuditRecord] {
        &self.records
    }

    /// Install the recording forwarder.
    pub fn register(methods: &mut Methods<S, Self>) {
        methods.on_raw_async(WILDCARD_METHOD, |entity, method, payload, cx| {
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
            let receipt = target.forward_shared(method, payload.to_vec(), cx);
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

impl<S, C> SharedEntitySource<S> for Audited<S, C>
where
    S: SharedSpec,
    S::Snapshot: Clone,
    C: SharedCaller<S>,
{
    fn snapshot(&self, cx: &App) -> S::Snapshot {
        self.target
            .shared_replica()
            .read(cx)
            .state
            .clone()
            .unwrap_or_else(|| self.fallback.clone())
    }
}
