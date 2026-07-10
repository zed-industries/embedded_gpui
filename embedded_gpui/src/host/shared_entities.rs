//! Host-side shared entities: homes with dynamic dispatch tables keyed by method name, and
//! the bookkeeping for remotes to guest-homed entities. See the "Shared entities" section
//! of `wit/plugin.wit`.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use embedded_gpui::{
    HandlerResponse, MethodHandler, Methods, RemoteSignal, ResponseSender, SharedSpec,
};
use gpui::{AnyEntity, App, Entity, Subscription};

use crate::bindings;

pub(crate) struct HostSharedEntity {
    name: String,
    type_name: &'static str,
    methods: HashMap<String, MethodHandler>,
    /// Whether the guest holds a live remote; events only flow while true.
    pub subscribed: bool,
    /// Anonymous shares keep their entity alive until released; named shares borrow.
    pub strong: Option<AnyEntity>,
    /// The notify observation plus any typed-event forwarders wired at share time.
    _subscriptions: Vec<Subscription>,
}

impl HostSharedEntity {
    pub fn new<S: SharedSpec, T: 'static>(
        name: String,
        methods: Methods<S, T>,
        strong: Option<AnyEntity>,
        subscriptions: Vec<Subscription>,
    ) -> Self {
        Self {
            name,
            type_name: S::TYPE_NAME,
            methods: methods.into_map(),
            subscribed: false,
            strong,
            _subscriptions: subscriptions,
        }
    }
}

pub(crate) struct PendingSend {
    pub request_id: Option<u64>,
    pub method: String,
    pub payload: Vec<u8>,
}

pub(crate) struct HostProjection {
    type_name: &'static str,
    pub entity_id: Option<u64>,
    /// Where incoming events land; every `Remote` for this entity holds it.
    pub signal: Entity<RemoteSignal>,
    /// Messages sent before the home side's announcement arrived; flushed in order, which
    /// is what makes sends to a not-yet-resolved entity pipeline correctly.
    pub pending_sends: Vec<PendingSend>,
}

#[derive(Default)]
pub(crate) struct HostShared {
    next_entity_id: u64,
    homes: HashMap<u64, HostSharedEntity>,
    pub projections_by_name: HashMap<String, HostProjection>,
    pub projection_names_by_id: HashMap<u64, String>,
    /// Guest announcements that arrived before the host attached a remote.
    pub unclaimed_announcements: HashMap<String, bindings::SharedEntityAnnouncement>,
    pub next_request_id: u64,
    pub pending_responses: HashMap<u64, ResponseSender>,
}

impl HostShared {
    /// Mint an entity id before the entity record exists, so event-forwarding
    /// subscriptions can capture it.
    pub fn reserve_entity_id(&mut self) -> u64 {
        self.next_entity_id += 1;
        self.next_entity_id
    }

    pub fn insert_home(&mut self, entity_id: u64, entity: HostSharedEntity) {
        self.homes.insert(entity_id, entity);
    }

    pub fn home_mut(&mut self, entity_id: u64) -> Option<&mut HostSharedEntity> {
        self.homes.get_mut(&entity_id)
    }

    pub fn insert_projection<S: SharedSpec>(&mut self, name: String, signal: Entity<RemoteSignal>) {
        self.insert_projection_inner::<S>(name, signal, None);
    }

    /// Insert a projection already bound to an entity id (connected from a ref).
    pub fn insert_projection_bound<S: SharedSpec>(
        &mut self,
        name: String,
        signal: Entity<RemoteSignal>,
        entity_id: u64,
    ) {
        self.insert_projection_inner::<S>(name.clone(), signal, Some(entity_id));
        self.projection_names_by_id.insert(entity_id, name);
    }

    fn insert_projection_inner<S: SharedSpec>(
        &mut self,
        name: String,
        signal: Entity<RemoteSignal>,
        entity_id: Option<u64>,
    ) {
        self.projections_by_name.insert(
            name,
            HostProjection {
                type_name: S::TYPE_NAME,
                entity_id,
                signal,
                pending_sends: Vec::new(),
            },
        );
    }

    /// Bind a guest announcement to a waiting projection. Returns the sends queued while
    /// unresolved, in order, ready to be pipelined to the guest.
    pub fn bind_projection(
        &mut self,
        announcement: &bindings::SharedEntityAnnouncement,
    ) -> Option<Vec<PendingSend>> {
        let Some(projection) = self.projections_by_name.get_mut(&announcement.name) else {
            self.unclaimed_announcements
                .insert(announcement.name.clone(), announcement.clone());
            return None;
        };
        if projection.type_name != announcement.type_name {
            log::error!(
                "embedded_gpui: shared entity {:?} is a {} in the guest but bound as {} here",
                announcement.name,
                announcement.type_name,
                projection.type_name
            );
            return None;
        }
        projection.entity_id = Some(announcement.entity_id);
        self.projection_names_by_id
            .insert(announcement.entity_id, announcement.name.clone());
        Some(std::mem::take(&mut projection.pending_sends))
    }

    /// Run the handler and return its response: encoded bytes for synchronous handlers,
    /// or a task the caller must drive for asynchronous ones. Control methods
    /// (`$subscribe` / `$release`) are handled by the caller before dispatch.
    pub fn dispatch(
        &mut self,
        entity_id: u64,
        method: &str,
        payload: &[u8],
        cx: &mut App,
    ) -> Result<HandlerResponse> {
        let home = self
            .homes
            .get_mut(&entity_id)
            .ok_or_else(|| anyhow!("message for unknown shared entity {entity_id}"))?;
        let handler = home
            .methods
            .get(method)
            .or_else(|| home.methods.get(embedded_gpui::WILDCARD_METHOD))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "shared entity {:?} ({}) has no method {method:?}",
                    home.name,
                    home.type_name
                )
            })?;
        let name = home.name.clone();
        Ok(match handler(method, payload, cx) {
            HandlerResponse::Ready(result) => HandlerResponse::Ready(
                result.with_context(|| format!("dispatching {method:?} to shared entity {name:?}")),
            ),
            pending => pending,
        })
    }
}
