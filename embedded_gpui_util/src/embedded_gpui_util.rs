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
