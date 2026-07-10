//! Host side of the "GPUI embedded in GPUI" spike. See `DESIGN.md` for the architecture and
//! `wit/plugin.wit` for the wire protocol. This crate compiles a `wasm32-wasip2` guest
//! component that renders a GPUI UI, and replays its retained display lists inside a native
//! GPUI application.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    EventSink, HandlerResponse, Methods, NOTIFY_EVENT, RELEASE_METHOD, RawSharedEvent, Remote,
    RemoteSignal, ResponseSender, SUBSCRIBE_METHOD, SharedHome, SharedRef, SharedSpec, Transport,
};
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, Context, Entity, Pixels, PlatformTextSystem, Size, Task, px};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
    });
}

use bindings::{Plugin, PluginImports};

mod plugin_element;
mod shared_entities;

pub use plugin_element::PluginViewState;

/// Dropping the last [`Remote`] for a ref-derived projection queues a release; the
/// host can't call into the guest from `Drop` (no context, and the instance may be
/// mid-call), so the queue is drained on the next `apply_effects` or [`PluginHost::pump`].
struct HostReleaseGuard {
    name: String,
    queue: Rc<RefCell<Vec<String>>>,
}

impl Drop for HostReleaseGuard {
    fn drop(&mut self) {
        self.queue.borrow_mut().push(self.name.clone());
    }
}

/// Effects drained from the guest after each call into it. The host acts on these once the
/// guest call has returned, never re-entering wasm from within a host import (see DESIGN.md
/// invariant 3).
#[derive(Default)]
pub struct PendingEffects {
    pub scene_updates: Vec<(u32, bindings::DisplayList)>,
    pub tick_delay_ms: Option<u32>,
    pub cursor_style: Option<gpui::CursorStyle>,
    pub shared_messages: Vec<bindings::SharedMessage>,
    pub shared_announcements: Vec<bindings::SharedEntityAnnouncement>,
    pub shared_events: Vec<bindings::SharedEvent>,
    pub shared_responses: Vec<bindings::SharedResponse>,
}

/// Alias used for the value returned from the `PluginInstance` methods after they drain the
/// pending effects.
pub type Effects = PendingEffects;

/// The data carried on the wasmtime `Store`. Host imports only mutate `pending`; the host
/// drains it after each guest call returns.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    text_system: Arc<dyn PlatformTextSystem>,
    pending: PendingEffects,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wasmtime::component::HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl HostState {
    fn font(&self, family: impl Into<gpui::SharedString>, weight: f32, italic: bool) -> gpui::Font {
        gpui::Font {
            family: family.into(),
            features: gpui::FontFeatures::default(),
            fallbacks: None,
            weight: gpui::FontWeight(weight),
            style: if italic {
                gpui::FontStyle::Italic
            } else {
                gpui::FontStyle::Normal
            },
        }
    }
}

impl PluginImports for HostState {
    fn resolve_font(&mut self, font: bindings::FontDescriptor) -> u32 {
        let requested = self.font(font.family.clone(), font.weight, font.italic);
        match self.text_system.font_id(&requested) {
            Ok(id) => return id.0 as u32,
            Err(error) => {
                log::warn!(
                    "embedded_gpui: failed to resolve font {:?}: {error:#}; falling back",
                    font.family
                );
            }
        }

        for fallback in [".SystemUIFont", "Helvetica"] {
            let candidate = self.font(fallback, font.weight, font.italic);
            match self.text_system.font_id(&candidate) {
                Ok(id) => return id.0 as u32,
                Err(error) => {
                    log::warn!("embedded_gpui: fallback font {fallback:?} unavailable: {error:#}");
                }
            }
        }

        log::error!("embedded_gpui: no fallback font available; using font id 0");
        0
    }

    fn font_metrics_for(&mut self, font_id: u32) -> bindings::FontMetrics {
        let metrics = self
            .text_system
            .font_metrics(gpui::FontId(font_id as usize));
        bindings::FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: bounds_from_f32(metrics.bounding_box),
        }
    }

    fn layout_line(
        &mut self,
        text: String,
        font_size: f32,
        runs: Vec<bindings::FontRun>,
    ) -> bindings::LineLayout {
        let runs: Vec<gpui::FontRun> = runs
            .into_iter()
            .map(|run| gpui::FontRun {
                len: run.len as usize,
                font_id: gpui::FontId(run.font_id as usize),
            })
            .collect();
        let layout = self.text_system.layout_line(&text, px(font_size), &runs);
        convert_line_layout(&layout)
    }

    fn advance(&mut self, font_id: u32, glyph_id: u32) -> bindings::Extent {
        match self
            .text_system
            .advance(gpui::FontId(font_id as usize), gpui::GlyphId(glyph_id))
        {
            Ok(advance) => bindings::Extent {
                width: advance.width,
                height: advance.height,
            },
            Err(error) => {
                log::warn!("embedded_gpui: advance failed for glyph {glyph_id}: {error:#}");
                bindings::Extent {
                    width: 0.,
                    height: 0.,
                }
            }
        }
    }

    fn typographic_bounds(&mut self, font_id: u32, glyph_id: u32) -> bindings::Bounds {
        match self
            .text_system
            .typographic_bounds(gpui::FontId(font_id as usize), gpui::GlyphId(glyph_id))
        {
            Ok(bounds) => bounds_from_f32(bounds),
            Err(error) => {
                log::warn!(
                    "embedded_gpui: typographic_bounds failed for glyph {glyph_id}: {error:#}"
                );
                bounds_from_f32(gpui::Bounds::default())
            }
        }
    }

    fn glyph_for_char(&mut self, font_id: u32, ch: char) -> Option<u32> {
        self.text_system
            .glyph_for_char(gpui::FontId(font_id as usize), ch)
            .map(|glyph| glyph.0)
    }

    fn glyph_raster_bounds(&mut self, params: bindings::GlyphParams) -> bindings::DeviceBounds {
        let request = gpui::RenderGlyphParams {
            font_id: gpui::FontId(params.font_id as usize),
            glyph_id: gpui::GlyphId(params.glyph_id),
            font_size: px(params.font_size),
            subpixel_variant: gpui::Point {
                x: params.subpixel_variant_x,
                y: params.subpixel_variant_y,
            },
            scale_factor: params.scale_factor,
            is_emoji: params.is_emoji,
            subpixel_rendering: false,
            dilation: 0,
        };
        match self.text_system.glyph_raster_bounds(&request) {
            Ok(bounds) => bindings::DeviceBounds {
                origin_x: bounds.origin.x.0,
                origin_y: bounds.origin.y.0,
                width: bounds.size.width.0,
                height: bounds.size.height.0,
            },
            Err(error) => {
                log::warn!(
                    "embedded_gpui: glyph_raster_bounds failed for glyph {}: {error:#}",
                    params.glyph_id
                );
                bindings::DeviceBounds {
                    origin_x: 0,
                    origin_y: 0,
                    width: 0,
                    height: 0,
                }
            }
        }
    }

    fn request_tick(&mut self, delay_ms: u32) {
        self.pending.tick_delay_ms = Some(match self.pending.tick_delay_ms {
            Some(existing) => existing.min(delay_ms),
            None => delay_ms,
        });
    }

    fn update_scene(&mut self, view_id: u32, list: bindings::DisplayList) {
        self.pending.scene_updates.push((view_id, list));
    }

    fn send_shared_message(&mut self, message: bindings::SharedMessage) {
        self.pending.shared_messages.push(message);
    }

    fn announce_shared_entity(&mut self, announcement: bindings::SharedEntityAnnouncement) {
        self.pending.shared_announcements.push(announcement);
    }

    fn emit_shared_event(&mut self, event: bindings::SharedEvent) {
        self.pending.shared_events.push(event);
    }

    fn send_shared_response(&mut self, response: bindings::SharedResponse) {
        self.pending.shared_responses.push(response);
    }

    fn set_cursor_style(&mut self, style: bindings::CursorStyle) {
        self.pending.cursor_style = Some(cursor_style_from_wire(style));
    }
}

fn bounds_from_f32(bounds: gpui::Bounds<f32>) -> bindings::Bounds {
    bindings::Bounds {
        origin: bindings::Point {
            x: bounds.origin.x,
            y: bounds.origin.y,
        },
        size: bindings::Extent {
            width: bounds.size.width,
            height: bounds.size.height,
        },
    }
}

fn convert_line_layout(layout: &gpui::LineLayout) -> bindings::LineLayout {
    bindings::LineLayout {
        font_size: f32::from(layout.font_size),
        width: f32::from(layout.width),
        ascent: f32::from(layout.ascent),
        descent: f32::from(layout.descent),
        len: layout.len as u32,
        runs: layout
            .runs
            .iter()
            .map(|run| bindings::ShapedRun {
                font_id: run.font_id.0 as u32,
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|glyph| bindings::ShapedGlyph {
                        id: glyph.id.0,
                        position: bindings::Point {
                            x: f32::from(glyph.position.x),
                            y: f32::from(glyph.position.y),
                        },
                        index: glyph.index as u32,
                        is_emoji: glyph.is_emoji,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn cursor_style_from_wire(style: bindings::CursorStyle) -> gpui::CursorStyle {
    match style {
        bindings::CursorStyle::Arrow => gpui::CursorStyle::Arrow,
        bindings::CursorStyle::Ibeam => gpui::CursorStyle::IBeam,
        bindings::CursorStyle::Crosshair => gpui::CursorStyle::Crosshair,
        bindings::CursorStyle::ClosedHand => gpui::CursorStyle::ClosedHand,
        bindings::CursorStyle::OpenHand => gpui::CursorStyle::OpenHand,
        bindings::CursorStyle::PointingHand => gpui::CursorStyle::PointingHand,
        bindings::CursorStyle::ResizeLeftRight => gpui::CursorStyle::ResizeLeftRight,
        bindings::CursorStyle::ResizeUpDown => gpui::CursorStyle::ResizeUpDown,
        bindings::CursorStyle::OperationNotAllowed => gpui::CursorStyle::OperationNotAllowed,
    }
}

/// A synchronous wasmtime store plus its instantiated bindings. Each method calls a guest
/// export and then drains and returns the effects the guest queued during that call.
pub struct PluginInstance {
    store: Store<HostState>,
    bindings: Plugin,
}

/// Grants extra WASI authority to a plugin's sandbox at instantiation.
pub type ConfigureWasi = Box<dyn FnOnce(&mut WasiCtxBuilder) + Send>;

/// Everything an embedder decides about a plugin's environment.
pub struct PluginOptions {
    /// Shapes glyph layout for the guest; usually the host platform's own text system,
    /// so plugin text is indistinguishable from native text.
    pub text_system: Arc<dyn PlatformTextSystem>,
    /// Configure the WASI sandbox the plugin runs in. The default grants nothing but
    /// inherited stdout/stderr; every additional authority (filesystem, network, env)
    /// is an explicit choice made here.
    pub configure_wasi: Option<ConfigureWasi>,
}

impl PluginOptions {
    pub fn new(text_system: Arc<dyn PlatformTextSystem>) -> Self {
        Self {
            text_system,
            configure_wasi: None,
        }
    }

    /// Grant the plugin additional WASI authority (preopened dirs, env vars, ...).
    pub fn with_wasi(
        mut self,
        configure: impl FnOnce(&mut WasiCtxBuilder) + Send + 'static,
    ) -> Self {
        self.configure_wasi = Some(Box::new(configure));
        self
    }
}

impl PluginInstance {
    pub fn new(component_path: &Path, options: PluginOptions) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).context("creating wasmtime engine")?;

        let component = Component::from_file(&engine, component_path)
            .with_context(|| format!("loading component {}", component_path.display()))?;

        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).context("adding wasi to linker")?;
        Plugin::add_to_linker::<_, HostState>(&mut linker, |state| state)
            .context("adding plugin host imports to linker")?;

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.inherit_stdout().inherit_stderr();
        if let Some(configure) = options.configure_wasi {
            configure(&mut wasi_builder);
        }
        let wasi = wasi_builder.build();
        let state = HostState {
            wasi,
            table: ResourceTable::new(),
            text_system: options.text_system,
            pending: PendingEffects::default(),
        };
        let mut store = Store::new(&engine, state);
        let bindings = Plugin::instantiate(&mut store, &component, &linker)
            .context("instantiating plugin component")?;

        Ok(Self { store, bindings })
    }

    fn take_effects(&mut self) -> Effects {
        std::mem::take(&mut self.store.data_mut().pending)
    }

    pub fn init(&mut self) -> Result<Effects> {
        self.bindings.call_init_plugin(&mut self.store)?;
        Ok(self.take_effects())
    }

    pub fn create_view(
        &mut self,
        view_id: u32,
        name: &str,
        size: Size<Pixels>,
        scale: f32,
    ) -> Result<Effects> {
        let extent = extent_from_size(size);
        self.bindings
            .call_create_view(&mut self.store, view_id, name, extent, scale)?;
        Ok(self.take_effects())
    }

    pub fn resize_view(&mut self, view_id: u32, size: Size<Pixels>, scale: f32) -> Result<Effects> {
        let extent = extent_from_size(size);
        self.bindings
            .call_resize_view(&mut self.store, view_id, extent, scale)?;
        Ok(self.take_effects())
    }

    pub fn handle_mouse(&mut self, view_id: u32, event: bindings::MouseEvent) -> Result<Effects> {
        self.bindings
            .call_handle_mouse(&mut self.store, view_id, event)?;
        Ok(self.take_effects())
    }

    pub fn handle_key(&mut self, view_id: u32, event: bindings::KeyEvent) -> Result<Effects> {
        self.bindings
            .call_handle_key(&mut self.store, view_id, &event)?;
        Ok(self.take_effects())
    }

    pub fn tick(&mut self) -> Result<Effects> {
        self.bindings.call_tick(&mut self.store)?;
        Ok(self.take_effects())
    }

    pub fn announce_shared_entity(
        &mut self,
        announcement: &bindings::SharedEntityAnnouncement,
    ) -> Result<Effects> {
        self.bindings
            .call_shared_entity_announced(&mut self.store, announcement)?;
        Ok(self.take_effects())
    }

    pub fn deliver_shared_event(&mut self, event: &bindings::SharedEvent) -> Result<Effects> {
        self.bindings
            .call_deliver_shared_event(&mut self.store, event)?;
        Ok(self.take_effects())
    }

    pub fn deliver_shared_message(&mut self, message: &bindings::SharedMessage) -> Result<Effects> {
        self.bindings
            .call_deliver_shared_message(&mut self.store, message)?;
        Ok(self.take_effects())
    }

    pub fn deliver_shared_response(
        &mut self,
        response: &bindings::SharedResponse,
    ) -> Result<Effects> {
        self.bindings
            .call_deliver_shared_response(&mut self.store, response)?;
        Ok(self.take_effects())
    }
}

fn extent_from_size(size: Size<Pixels>) -> bindings::Extent {
    bindings::Extent {
        width: f32::from(size.width),
        height: f32::from(size.height),
    }
}

/// A GPUI entity that owns the wasmtime store and mediates between the host application and
/// the guest. All calls into the guest happen from here, on the foreground thread.
/// Images shipped by the guest, cached per instance and shared by all of its views.
pub type PluginImages = Rc<RefCell<HashMap<u64, Arc<gpui::RenderImage>>>>;

/// One call into the guest, queued for the background worker that owns the store.
enum PluginRequest {
    Init,
    CreateView {
        view_id: u32,
        name: String,
        size: Size<Pixels>,
        scale: f32,
    },
    ResizeView {
        view_id: u32,
        size: Size<Pixels>,
        scale: f32,
    },
    HandleMouse {
        view_id: u32,
        event: bindings::MouseEvent,
    },
    HandleKey {
        view_id: u32,
        event: bindings::KeyEvent,
    },
    Tick,
    AnnounceSharedEntity(bindings::SharedEntityAnnouncement),
    DeliverSharedMessage(bindings::SharedMessage),
    DeliverSharedEvent(bindings::SharedEvent),
    DeliverSharedResponse(bindings::SharedResponse),
}

impl PluginInstance {
    fn handle(&mut self, request: PluginRequest) -> Result<Effects> {
        match request {
            PluginRequest::Init => self.init(),
            PluginRequest::CreateView {
                view_id,
                name,
                size,
                scale,
            } => self.create_view(view_id, &name, size, scale),
            PluginRequest::ResizeView {
                view_id,
                size,
                scale,
            } => self.resize_view(view_id, size, scale),
            PluginRequest::HandleMouse { view_id, event } => self.handle_mouse(view_id, event),
            PluginRequest::HandleKey { view_id, event } => self.handle_key(view_id, event),
            PluginRequest::Tick => self.tick(),
            PluginRequest::AnnounceSharedEntity(announcement) => {
                self.announce_shared_entity(&announcement)
            }
            PluginRequest::DeliverSharedMessage(message) => self.deliver_shared_message(&message),
            PluginRequest::DeliverSharedEvent(event) => self.deliver_shared_event(&event),
            PluginRequest::DeliverSharedResponse(response) => {
                self.deliver_shared_response(&response)
            }
        }
    }
}

pub struct PluginHost {
    /// Requests to the background worker that owns the wasmtime store. FIFO: the worker
    /// processes one call at a time, and each call's effects come back in order.
    requests: mpsc::UnboundedSender<PluginRequest>,
    views: HashMap<u32, Entity<PluginViewState>>,
    views_by_name: HashMap<String, Entity<PluginViewState>>,
    next_view_id: u32,
    images: PluginImages,
    shared: shared_entities::HostShared,
    /// Names whose `HostReleaseGuard` dropped; drained into `$release` sends.
    pending_releases: Rc<RefCell<Vec<String>>>,
    release_guards: HashMap<String, std::rc::Weak<HostReleaseGuard>>,
    scheduled_tick: Option<Task<()>>,
    _worker: Task<()>,
    _pump: Task<()>,
}

impl PluginHost {
    /// Move `instance` onto a background worker and wire the effect pump. A slow or
    /// misbehaving guest can no longer stall the UI thread: calls into wasm happen on
    /// the worker, strictly one at a time, and their effects are applied back on the
    /// foreground in the same order.
    pub fn new(mut instance: PluginInstance, cx: &mut Context<Self>) -> Self {
        let (requests, mut request_rx) = mpsc::unbounded::<PluginRequest>();
        let (effects_tx, mut effects_rx) = mpsc::unbounded::<Effects>();

        let worker = cx.background_spawn(async move {
            while let Some(request) = request_rx.next().await {
                match instance.handle(request) {
                    Ok(effects) => {
                        if effects_tx.unbounded_send(effects).is_err() {
                            break;
                        }
                    }
                    Err(error) => log::error!("embedded_gpui: plugin call failed: {error:#}"),
                }
            }
        });

        let pump = cx.spawn(async move |host, cx| {
            while let Some(effects) = effects_rx.next().await {
                if host
                    .update(cx, |host, cx| host.apply_effects(effects, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let this = Self {
            requests,
            views: HashMap::new(),
            views_by_name: HashMap::new(),
            next_view_id: 0,
            images: PluginImages::default(),
            shared: shared_entities::HostShared::default(),
            pending_releases: Rc::default(),
            release_guards: HashMap::new(),
            scheduled_tick: None,
            _worker: worker,
            _pump: pump,
        };
        this.enqueue(PluginRequest::Init);
        this
    }

    /// The whole embedding story in one call: compile and instantiate the component on
    /// a background thread, then hand back a ready [`PluginHost`]. Pair with
    /// [`PluginHost::view`] to place the plugin's surfaces in your UI.
    pub fn load(
        path: std::path::PathBuf,
        options: PluginOptions,
        cx: &mut gpui::App,
    ) -> Task<Result<Entity<PluginHost>>> {
        let instance = cx.background_spawn(async move { PluginInstance::new(&path, options) });
        cx.spawn(async move |cx| {
            let instance = instance.await?;
            Ok(cx.update(|cx| cx.new(|cx| PluginHost::new(instance, cx))))
        })
    }

    fn enqueue(&self, request: PluginRequest) {
        if self.requests.unbounded_send(request).is_err() {
            log::error!("embedded_gpui: plugin worker is gone; dropping request");
        }
    }

    /// Share a host entity with the guest under a well-known name (a mount the guest
    /// attaches to with `remote`). The entity becomes the *home* of the shared state:
    /// its `#[shared_home]` methods answer guest messages, and its `cx.notify` /
    /// declared `cx.emit` events reach every guest remote.
    pub fn share<S, T>(
        &mut self,
        entity: &Entity<T>,
        name: impl Into<String>,
        cx: &mut Context<Self>,
    ) where
        S: SharedSpec,
        T: SharedHome<S>,
    {
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let entity_id = self.shared.reserve_entity_id();
        let events = T::events(entity, self.home_event_sink(entity_id, cx), cx);
        self.install_home(
            entity,
            methods,
            events,
            None,
            name.into(),
            true,
            entity_id,
            cx,
        );
    }

    /// The dynamic escape hatch beneath [`PluginHost::share`]: register method handlers
    /// with a closure instead of a schema interface. `cx.notify` still crosses; typed
    /// events are not wired (implement [`SharedHome`] manually if you need both).
    pub fn share_with<S, T>(
        &mut self,
        entity: &Entity<T>,
        name: impl Into<String>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut Context<Self>,
    ) where
        S: SharedSpec,
        T: 'static,
    {
        let mut methods = Methods::new(entity.downgrade());
        register(&mut methods);
        let entity_id = self.shared.reserve_entity_id();
        self.install_home(
            entity,
            methods,
            Vec::new(),
            None,
            name.into(),
            true,
            entity_id,
            cx,
        );
    }

    /// Share a host entity anonymously, returning a capability reference to embed in
    /// message or event payloads. The home holds a strong handle to the entity until the
    /// reference is released.
    pub fn share_anonymous<S, T>(
        &mut self,
        entity: &Entity<T>,
        cx: &mut Context<Self>,
    ) -> SharedRef<S>
    where
        S: SharedSpec,
        T: SharedHome<S>,
    {
        let mut methods = Methods::new(entity.downgrade());
        T::methods(&mut methods);
        let entity_id = self.shared.reserve_entity_id();
        let events = T::events(entity, self.home_event_sink(entity_id, cx), cx);
        self.install_home(
            entity,
            methods,
            events,
            Some(entity.clone().into_any()),
            format!("#{entity_id}"),
            false,
            entity_id,
            cx,
        );
        SharedRef::from_raw(entity_id)
    }

    /// [`PluginHost::share_anonymous`] with a closure-registered dispatch table; see
    /// [`PluginHost::share_with`].
    pub fn share_anonymous_with<S, T>(
        &mut self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut Context<Self>,
    ) -> SharedRef<S>
    where
        S: SharedSpec,
        T: 'static,
    {
        let mut methods = Methods::new(entity.downgrade());
        register(&mut methods);
        let entity_id = self.shared.reserve_entity_id();
        self.install_home(
            entity,
            methods,
            Vec::new(),
            Some(entity.clone().into_any()),
            format!("#{entity_id}"),
            false,
            entity_id,
            cx,
        );
        SharedRef::from_raw(entity_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn install_home<S, T>(
        &mut self,
        entity: &Entity<T>,
        methods: Methods<S, T>,
        event_forwarders: Vec<gpui::Subscription>,
        strong: Option<gpui::AnyEntity>,
        name: String,
        announce: bool,
        entity_id: u64,
        cx: &mut Context<Self>,
    ) where
        S: SharedSpec,
        T: 'static,
    {
        let mut subscriptions = vec![cx.observe(entity, move |host, _, _| {
            host.emit_home_event(entity_id, NOTIFY_EVENT, Vec::new());
        })];
        subscriptions.extend(event_forwarders);
        self.shared.insert_home(
            entity_id,
            shared_entities::HostSharedEntity::new(name.clone(), methods, strong, subscriptions),
        );
        if announce {
            self.enqueue(PluginRequest::AnnounceSharedEntity(
                bindings::SharedEntityAnnouncement {
                    entity_id,
                    type_name: S::TYPE_NAME.to_string(),
                    name,
                },
            ));
        }
    }

    /// The sink handed to schema-generated event wiring: it moves a home's typed events
    /// onto the wire.
    fn home_event_sink(&self, entity_id: u64, cx: &mut Context<Self>) -> EventSink {
        let host = cx.weak_entity();
        Rc::new(move |event: &str, payload: Vec<u8>, cx: &mut gpui::App| {
            let event = event.to_string();
            host.update(cx, |host, _| {
                host.emit_home_event(entity_id, &event, payload)
            })
            .ok();
        })
    }

    /// Attach to a guest-homed shared entity by name. Returns immediately; sends queue
    /// (in order) until the guest's announcement arrives, then flush.
    pub fn remote<S: SharedSpec>(
        &mut self,
        name: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Remote<S> {
        let name = name.into();
        let signal = cx.new(|_| RemoteSignal::new());
        self.shared
            .insert_projection::<S>(name.clone(), signal.clone());
        if let Some(announcement) = self.shared.unclaimed_announcements.remove(&name) {
            self.bind_projection(announcement);
        }
        self.send_to_guest(&name, SUBSCRIBE_METHOD, Vec::new(), None);
        Remote::from_parts(signal, self.remote_transport(name, cx), None)
    }

    /// Attach to a guest-homed shared entity through a capability reference received in
    /// a payload. No name is involved: the ref's id addresses the entity directly.
    /// Connecting the same ref twice returns a handle to the same projection; when the
    /// last clone drops, the home is told to release the entity.
    pub fn connect<S: SharedSpec>(
        &mut self,
        reference: SharedRef<S>,
        cx: &mut Context<Self>,
    ) -> Remote<S> {
        let entity_id = reference.entity_id();
        let name = format!("#{entity_id}");
        if let Some(guard) = self
            .release_guards
            .get(&name)
            .and_then(std::rc::Weak::upgrade)
            && let Some(projection) = self.shared.projections_by_name.get(&name)
        {
            let signal = projection.signal.clone();
            return Remote::from_parts(signal, self.remote_transport(name, cx), Some(guard));
        }
        // A dead guard whose release hasn't been drained yet means the projection state
        // is stale; releasing now and resubscribing below keeps the guest home consistent.
        if self.shared.projections_by_name.contains_key(&name) {
            self.pending_releases
                .borrow_mut()
                .retain(|pending| pending != &name);
            self.release_projection(&name, cx);
        }
        let signal = cx.new(|_| RemoteSignal::new());
        self.shared
            .insert_projection_bound::<S>(name.clone(), signal.clone(), entity_id);
        self.send_to_guest(&name, SUBSCRIBE_METHOD, Vec::new(), None);
        let guard = Rc::new(HostReleaseGuard {
            name: name.clone(),
            queue: self.pending_releases.clone(),
        });
        self.release_guards
            .insert(name.clone(), Rc::downgrade(&guard));
        Remote::from_parts(signal, self.remote_transport(name, cx), Some(guard))
    }

    /// All outgoing remote traffic is deferred rather than dispatched inline: a handler
    /// on a host-homed entity runs inside a `PluginHost` update, so a synchronous
    /// re-entry (e.g. a caretaker forwarding from its wildcard handler) would
    /// double-borrow the host. Deferral keeps sends FIFO while making remotes safe to
    /// use from anywhere.
    fn remote_transport(&self, name: String, cx: &mut Context<Self>) -> Rc<Transport> {
        let host = cx.weak_entity();
        Rc::new(
            move |method: &str, payload: Vec<u8>, response: Option<ResponseSender>, cx| {
                let host = host.clone();
                let name = name.clone();
                let method = method.to_string();
                cx.defer(move |cx| {
                    host.update(cx, |host, _| {
                        host.send_to_guest(&name, &method, payload, response);
                    })
                    .ok();
                });
            },
        )
    }

    /// Send `$release` for a ref-derived projection and forget it locally. The guest home
    /// drops its strong handle; events stop flowing.
    fn release_projection(&mut self, name: &str, _cx: &mut Context<Self>) {
        self.send_to_guest(name, RELEASE_METHOD, Vec::new(), None);
        if let Some(projection) = self.shared.projections_by_name.remove(name)
            && let Some(entity_id) = projection.entity_id
        {
            self.shared.projection_names_by_id.remove(&entity_id);
        }
        self.release_guards.remove(name);
    }

    fn drain_pending_releases(&mut self, cx: &mut Context<Self>) {
        loop {
            let names = std::mem::take(&mut *self.pending_releases.borrow_mut());
            if names.is_empty() {
                break;
            }
            for name in names {
                self.release_projection(&name, cx);
            }
        }
    }

    /// Flush deferred work (queued capability releases) and give the guest a turn. Hosts
    /// with quiescent plugins (no pending tick) can call this to make drops observable.
    pub fn pump(&mut self, cx: &mut Context<Self>) {
        self.drain_pending_releases(cx);
        self.tick(cx);
    }

    /// Send one event from a host home to the guest's remotes, if the guest holds any.
    fn emit_home_event(&mut self, entity_id: u64, event: &str, payload: Vec<u8>) {
        let Some(home) = self.shared.home_mut(entity_id) else {
            return;
        };
        if home.subscribed {
            self.enqueue(PluginRequest::DeliverSharedEvent(bindings::SharedEvent {
                entity_id,
                name: event.to_string(),
                payload,
            }));
        }
    }

    fn bind_projection(&mut self, announcement: bindings::SharedEntityAnnouncement) {
        if let Some(pending_sends) = self.shared.bind_projection(&announcement) {
            for send in pending_sends {
                self.deliver_message_to_guest(announcement.entity_id, send);
            }
        }
    }

    fn deliver_message_to_guest(&mut self, entity_id: u64, send: shared_entities::PendingSend) {
        self.enqueue(PluginRequest::DeliverSharedMessage(
            bindings::SharedMessage {
                entity_id,
                request_id: send.request_id,
                method: send.method,
                payload: send.payload,
            },
        ));
    }

    fn send_to_guest(
        &mut self,
        name: &str,
        method: &str,
        payload: Vec<u8>,
        response: Option<ResponseSender>,
    ) {
        if !self.shared.projections_by_name.contains_key(name) {
            // Dropping `response` here resolves the caller's receipt with an error.
            log::warn!("embedded_gpui: send to unknown shared entity {name:?}");
            return;
        }
        let request_id = response.map(|sender| {
            self.shared.next_request_id += 1;
            let request_id = self.shared.next_request_id;
            self.shared.pending_responses.insert(request_id, sender);
            request_id
        });
        let send = shared_entities::PendingSend {
            request_id,
            method: method.to_string(),
            payload,
        };
        let Some(projection) = self.shared.projections_by_name.get_mut(name) else {
            return;
        };
        match projection.entity_id {
            Some(entity_id) => self.deliver_message_to_guest(entity_id, send),
            None => projection.pending_sends.push(send),
        }
    }

    /// The view named `name` from the plugin, as a GPUI view: place it anywhere in
    /// your element tree and it fills its slot. Creation is lazy — the guest's
    /// `create-view` runs on first layout, with the measured slot size and the
    /// window's actual scale factor — and repeated calls return the same view.
    pub fn view(
        &mut self,
        name: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Entity<PluginViewState> {
        let name = name.into();
        if let Some(view) = self.views_by_name.get(&name) {
            return view.clone();
        }
        let view_id = self.next_view_id;
        self.next_view_id += 1;
        let host = cx.weak_entity();
        let images = self.images.clone();
        let view = cx.new(|cx| PluginViewState::new(view_id, name.clone(), host, images, cx));
        self.views.insert(view_id, view.clone());
        self.views_by_name.insert(name, view.clone());
        view
    }

    pub(crate) fn create_view_now(
        &mut self,
        view_id: u32,
        name: String,
        size: Size<Pixels>,
        scale: f32,
        _cx: &mut Context<Self>,
    ) {
        self.enqueue(PluginRequest::CreateView {
            view_id,
            name,
            size,
            scale,
        });
    }

    pub fn resize_view(
        &mut self,
        view_id: u32,
        size: Size<Pixels>,
        scale: f32,
        _cx: &mut Context<Self>,
    ) {
        self.enqueue(PluginRequest::ResizeView {
            view_id,
            size,
            scale,
        });
    }

    pub fn handle_mouse(
        &mut self,
        view_id: u32,
        event: bindings::MouseEvent,
        _cx: &mut Context<Self>,
    ) {
        self.enqueue(PluginRequest::HandleMouse { view_id, event });
    }

    pub fn handle_key(&mut self, view_id: u32, event: bindings::KeyEvent, _cx: &mut Context<Self>) {
        self.enqueue(PluginRequest::HandleKey { view_id, event });
    }

    fn tick(&mut self, _cx: &mut Context<Self>) {
        self.enqueue(PluginRequest::Tick);
    }

    fn apply_effects(&mut self, effects: Effects, cx: &mut Context<Self>) {
        self.drain_pending_releases(cx);

        for announcement in effects.shared_announcements {
            self.bind_projection(announcement);
        }

        // Events before responses, mirroring the order the home side produced them in.
        for event in effects.shared_events {
            self.apply_guest_event(event, cx);
        }

        for response in effects.shared_responses {
            let Some(sender) = self.shared.pending_responses.remove(&response.request_id) else {
                log::warn!(
                    "embedded_gpui: response for unknown request {}",
                    response.request_id
                );
                continue;
            };
            sender.send(response.outcome).ok();
        }

        for message in effects.shared_messages {
            self.apply_guest_message(message, cx);
        }

        for (view_id, list) in effects.scene_updates {
            self.ingest_images(&list);
            if let Some(view) = self.views.get(&view_id) {
                view.update(cx, |view, cx| {
                    view.set_display_list(list);
                    cx.notify();
                });
            } else {
                log::warn!("embedded_gpui: update-scene for unknown view {view_id}");
            }
        }

        if let Some(cursor) = effects.cursor_style {
            for view in self.views.values() {
                view.update(cx, |view, cx| {
                    view.set_cursor(cursor);
                    cx.notify();
                });
            }
        }

        if let Some(delay) = effects.tick_delay_ms {
            self.scheduled_tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(delay as u64))
                    .await;
                this.update(cx, |this, cx| this.tick(cx)).ok();
            }));
        }
    }

    /// Handle one guest message to a host home: control methods inline, everything else
    /// through the dispatch table.
    fn apply_guest_message(&mut self, message: bindings::SharedMessage, cx: &mut Context<Self>) {
        let entity_id = message.entity_id;
        let outcome = match message.method.as_str() {
            SUBSCRIBE_METHOD => {
                if let Some(home) = self.shared.home_mut(entity_id) {
                    home.subscribed = true;
                }
                // A subscription is answered with an initial notify, so a new remote's
                // observers always fire at least once.
                self.emit_home_event(entity_id, NOTIFY_EVENT, Vec::new());
                embedded_gpui::encode(&()).map_err(|error| format!("{error:#}"))
            }
            RELEASE_METHOD => {
                if let Some(home) = self.shared.home_mut(entity_id) {
                    home.subscribed = false;
                    home.strong = None;
                }
                embedded_gpui::encode(&()).map_err(|error| format!("{error:#}"))
            }
            method => match self
                .shared
                .dispatch(entity_id, method, &message.payload, cx)
            {
                Ok(HandlerResponse::Ready(result)) => result.map_err(|error| {
                    log::error!("embedded_gpui: shared message failed: {error:#}");
                    format!("{error:#}")
                }),
                Ok(HandlerResponse::Pending(task)) => {
                    // The handler's work outlives this delivery; the response flows when
                    // its task resolves.
                    let request_id = message.request_id;
                    cx.spawn(async move |host, cx| {
                        let outcome = task.await.map_err(|error| {
                            log::error!("embedded_gpui: shared message failed: {error:#}");
                            format!("{error:#}")
                        });
                        if let Some(request_id) = request_id {
                            host.update(cx, |host, _| {
                                host.deliver_response_to_guest(bindings::SharedResponse {
                                    request_id,
                                    outcome,
                                });
                            })
                            .ok();
                        }
                    })
                    .detach();
                    return;
                }
                Err(error) => {
                    log::error!("embedded_gpui: shared message failed: {error:#}");
                    Err(format!("{error:#}"))
                }
            },
        };
        if let Some(request_id) = message.request_id {
            // Deferred so the handler's own effects (sends, notifies) flush to the guest
            // before the response.
            let host = cx.weak_entity();
            cx.defer(move |cx| {
                host.update(cx, |host, _| {
                    host.deliver_response_to_guest(bindings::SharedResponse {
                        request_id,
                        outcome,
                    });
                })
                .ok();
            });
        }
    }

    fn deliver_response_to_guest(&mut self, response: bindings::SharedResponse) {
        self.enqueue(PluginRequest::DeliverSharedResponse(response));
    }

    fn apply_guest_event(&mut self, event: bindings::SharedEvent, cx: &mut Context<Self>) {
        let Some(name) = self
            .shared
            .projection_names_by_id
            .get(&event.entity_id)
            .cloned()
        else {
            log::warn!(
                "embedded_gpui: event for unknown shared entity {}",
                event.entity_id
            );
            return;
        };
        let Some(projection) = self.shared.projections_by_name.get(&name) else {
            return;
        };
        let signal = projection.signal.clone();
        if event.name == NOTIFY_EVENT {
            signal.update(cx, |_, cx| cx.notify());
        } else {
            signal.update(cx, |_, cx| {
                cx.emit(RawSharedEvent {
                    name: event.name,
                    payload: event.payload,
                })
            });
        }
    }

    /// Decode freshly shipped image payloads into `RenderImage`s. Bytes are premultiplied
    /// BGRA straight from the guest's atlas pipeline, so no conversion is needed: the host's
    /// atlas upload will read back exactly these bytes.
    fn ingest_images(&mut self, list: &bindings::DisplayList) {
        for payload in &list.new_images {
            let expected_len = payload.width as usize * payload.height as usize * 4;
            if payload.bytes.len() != expected_len {
                log::error!(
                    "embedded_gpui: image payload {} has {} bytes, expected {expected_len}",
                    payload.id,
                    payload.bytes.len()
                );
                continue;
            }
            let Some(buffer) =
                image::RgbaImage::from_raw(payload.width, payload.height, payload.bytes.clone())
            else {
                log::error!("embedded_gpui: image payload {} is malformed", payload.id);
                continue;
            };
            let render_image = Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(buffer)
            ]));
            self.images.borrow_mut().insert(payload.id, render_image);
        }
    }
}

/// [`PluginHost`]'s surface on the entity handle itself, so call sites skip the
/// `host.update(cx, |host, cx| ...)` ceremony: `host.view("panel", cx)`,
/// `host.share(&entity, "name", cx)`, `host.remote::<CounterApi>("clicks", cx)`.
pub trait PluginHostHandle {
    /// See [`PluginHost::share`].
    fn share<S: SharedSpec, T: SharedHome<S>>(
        &self,
        entity: &Entity<T>,
        name: impl Into<String>,
        cx: &mut gpui::App,
    );

    /// See [`PluginHost::share_with`].
    fn share_with<S: SharedSpec, T: 'static>(
        &self,
        entity: &Entity<T>,
        name: impl Into<String>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    );

    /// See [`PluginHost::share_anonymous`].
    fn share_anonymous<S: SharedSpec, T: SharedHome<S>>(
        &self,
        entity: &Entity<T>,
        cx: &mut gpui::App,
    ) -> SharedRef<S>;

    /// See [`PluginHost::share_anonymous_with`].
    fn share_anonymous_with<S: SharedSpec, T: 'static>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    ) -> SharedRef<S>;

    /// See [`PluginHost::remote`].
    fn remote<S: SharedSpec>(&self, name: impl Into<String>, cx: &mut gpui::App) -> Remote<S>;

    /// See [`PluginHost::connect`].
    fn connect<S: SharedSpec>(&self, reference: SharedRef<S>, cx: &mut gpui::App) -> Remote<S>;

    /// See [`PluginHost::view`].
    fn view(&self, name: impl Into<String>, cx: &mut gpui::App) -> Entity<PluginViewState>;

    /// See [`PluginHost::pump`].
    fn pump(&self, cx: &mut gpui::App);
}

impl PluginHostHandle for Entity<PluginHost> {
    fn share<S: SharedSpec, T: SharedHome<S>>(
        &self,
        entity: &Entity<T>,
        name: impl Into<String>,
        cx: &mut gpui::App,
    ) {
        self.update(cx, |host, cx| host.share(entity, name, cx))
    }

    fn share_with<S: SharedSpec, T: 'static>(
        &self,
        entity: &Entity<T>,
        name: impl Into<String>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    ) {
        self.update(cx, |host, cx| host.share_with(entity, name, register, cx))
    }

    fn share_anonymous<S: SharedSpec, T: SharedHome<S>>(
        &self,
        entity: &Entity<T>,
        cx: &mut gpui::App,
    ) -> SharedRef<S> {
        self.update(cx, |host, cx| host.share_anonymous(entity, cx))
    }

    fn share_anonymous_with<S: SharedSpec, T: 'static>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    ) -> SharedRef<S> {
        self.update(cx, |host, cx| {
            host.share_anonymous_with(entity, register, cx)
        })
    }

    fn remote<S: SharedSpec>(&self, name: impl Into<String>, cx: &mut gpui::App) -> Remote<S> {
        self.update(cx, |host, cx| host.remote(name, cx))
    }

    fn connect<S: SharedSpec>(&self, reference: SharedRef<S>, cx: &mut gpui::App) -> Remote<S> {
        self.update(cx, |host, cx| host.connect(reference, cx))
    }

    fn view(&self, name: impl Into<String>, cx: &mut gpui::App) -> Entity<PluginViewState> {
        self.update(cx, |host, cx| host.view(name, cx))
    }

    fn pump(&self, cx: &mut gpui::App) {
        self.update(cx, |host, cx| host.pump(cx))
    }
}
