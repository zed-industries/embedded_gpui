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

use crate::registry::{Objects, WireMessage, WireOutgoing, WireResponse};
use crate::{Interface, Methods, Ref, Remote, Shared};
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

pub use plugin_element::PluginViewState;

/// Effects drained from the guest after each call into it. The host acts on these once the
/// guest call has returned, never re-entering wasm from within a host import (see DESIGN.md
/// invariant 3).
#[derive(Default)]
pub struct PendingEffects {
    pub scene_updates: Vec<(u32, bindings::DisplayList)>,
    pub tick_delay_ms: Option<u32>,
    pub cursor_style: Option<gpui::CursorStyle>,
    pub messages: Vec<bindings::ObjectMessage>,
    pub responses: Vec<bindings::ObjectResponse>,
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

    fn send_object_message(&mut self, message: bindings::ObjectMessage) {
        self.pending.messages.push(message);
    }

    fn send_object_response(&mut self, response: bindings::ObjectResponse) {
        self.pending.responses.push(response);
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

    pub fn deliver_object_message(&mut self, message: &bindings::ObjectMessage) -> Result<Effects> {
        self.bindings
            .call_deliver_object_message(&mut self.store, message)?;
        Ok(self.take_effects())
    }

    pub fn deliver_object_response(
        &mut self,
        response: &bindings::ObjectResponse,
    ) -> Result<Effects> {
        self.bindings
            .call_deliver_object_response(&mut self.store, response)?;
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
    DeliverMessage(bindings::ObjectMessage),
    DeliverResponse(bindings::ObjectResponse),
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
            PluginRequest::DeliverMessage(message) => self.deliver_object_message(&message),
            PluginRequest::DeliverResponse(response) => self.deliver_object_response(&response),
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
    /// This end's object registry: the object model lives there, side-blind; this
    /// entity supplies only its transport (the request queue) and the wasm surface.
    objects: Objects,
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

        let sink = requests.clone();
        let objects = Objects::new(Box::new(move |outgoing| {
            let request = match outgoing {
                WireOutgoing::Message(message) => {
                    PluginRequest::DeliverMessage(bindings::ObjectMessage {
                        entity_id: message.entity_id,
                        request_id: message.request_id,
                        method: message.method,
                        payload: message.payload,
                    })
                }
                WireOutgoing::Response(response) => {
                    PluginRequest::DeliverResponse(bindings::ObjectResponse {
                        request_id: response.request_id,
                        outcome: response.outcome,
                    })
                }
            };
            if sink.unbounded_send(request).is_err() {
                log::error!("embedded_gpui: plugin worker is gone; dropping message");
            }
        }));

        // Object traffic is applied straight to the registry, *outside* any update of
        // this entity: handlers run with the host entity un-borrowed, so user code in a
        // handler may freely use `PluginHostHandle` (e.g. a root method sharing a new
        // entity). Only the wasm surface (scenes, cursor, ticks) goes through the
        // entity.
        let pump_objects = objects.clone();
        let pump = cx.spawn(async move |host, cx| {
            while let Some(mut effects) = effects_rx.next().await {
                let responses = std::mem::take(&mut effects.responses);
                let messages = std::mem::take(&mut effects.messages);
                let applied = cx.update(|cx| {
                    pump_objects.drain_releases();

                    for response in responses {
                        pump_objects.deliver_response(WireResponse {
                            request_id: response.request_id,
                            outcome: response.outcome,
                        });
                    }

                    for message in messages {
                        pump_objects.deliver_message(
                            WireMessage {
                                entity_id: message.entity_id,
                                request_id: message.request_id,
                                method: message.method,
                                payload: message.payload,
                            },
                            cx,
                        );
                    }

                    pump_objects.drain_releases();
                    host.update(cx, |host, cx| host.apply_effects(effects, cx))
                });
                if applied.is_err() {
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
            objects,
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

    /// Install `entity` as this end's root object: the single capability the other
    /// end starts from, whose methods return everything else. Root traffic that
    /// arrives before this call queues and is delivered in order once the root
    /// exists, so the two ends' bootstraps may race freely.
    pub fn share_root<S, T>(&mut self, entity: &Entity<T>, cx: &mut Context<Self>)
    where
        S: Interface,
        T: Shared<S>,
    {
        self.objects.share_root(entity, cx);
    }

    /// Share a local entity, returning a capability reference to embed in message or
    /// event payloads. The registry holds the entity alive until the other end's last
    /// remote drops.
    pub fn share<S, T>(&mut self, entity: &Entity<T>, cx: &mut Context<Self>) -> Ref<S>
    where
        S: Interface,
        T: Shared<S>,
    {
        self.objects.share(entity, cx)
    }

    /// [`PluginHost::share`] with a closure-registered dispatch table; the dynamic
    /// escape hatch. `cx.notify` still crosses; typed events are not wired (implement
    /// [`Shared`] manually if you need both).
    pub fn share_with<S, T>(
        &mut self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut Context<Self>,
    ) -> Ref<S>
    where
        S: Interface,
        T: 'static,
    {
        self.objects.share_with(entity, register, cx)
    }

    /// Attach to the other end's root object: the single typed capability the plugin
    /// starts everything from. Returns immediately (root traffic queues until the
    /// other end's root is installed), so the whole bootstrap is synchronous.
    pub fn root<S: Interface>(&mut self, _cx: &mut Context<Self>) -> Remote<S> {
        self.objects.root()
    }

    /// Attach to an entity through a capability reference received in a payload.
    pub fn connect<S: Interface>(
        &mut self,
        reference: Ref<S>,
        _cx: &mut Context<Self>,
    ) -> Remote<S> {
        self.objects.connect(reference)
    }

    /// Flush deferred work (queued capability releases) and give the guest a turn.
    /// Hosts with quiescent plugins (no pending tick) can call this to make drops
    /// observable.
    pub fn pump(&mut self, cx: &mut Context<Self>) {
        self.objects.drain_releases();
        self.tick(cx);
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

    /// Apply the wasm-surface effects of one guest turn: scenes, cursor, and the next
    /// tick. Object traffic was already applied to the registry by the pump, outside
    /// this entity's update.
    fn apply_effects(&mut self, effects: Effects, cx: &mut Context<Self>) {
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

/// The ergonomic surface of [`PluginHost`]: the same operations, callable directly on an
/// `Entity<PluginHost>` without the `update` ceremony. The object operations only read
/// the host entity (the registry is shared), so they are safe to call from anywhere,
/// including inside method handlers.
pub trait PluginHostHandle {
    /// See [`PluginHost::share_root`].
    fn share_root<S: Interface, T: Shared<S>>(&self, entity: &Entity<T>, cx: &mut gpui::App);

    /// See [`PluginHost::share`].
    fn share<S: Interface, T: Shared<S>>(&self, entity: &Entity<T>, cx: &mut gpui::App) -> Ref<S>;

    /// See [`PluginHost::share_with`].
    fn share_with<S: Interface, T: 'static>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    ) -> Ref<S>;

    /// See [`PluginHost::root`].
    fn root<S: Interface>(&self, cx: &mut gpui::App) -> Remote<S>;

    /// See [`PluginHost::connect`].
    fn connect<S: Interface>(&self, reference: Ref<S>, cx: &mut gpui::App) -> Remote<S>;

    /// See [`PluginHost::view`].
    fn view(&self, name: impl Into<String>, cx: &mut gpui::App) -> Entity<PluginViewState>;

    /// See [`PluginHost::pump`].
    fn pump(&self, cx: &mut gpui::App);
}

impl PluginHostHandle for Entity<PluginHost> {
    fn share_root<S: Interface, T: Shared<S>>(&self, entity: &Entity<T>, cx: &mut gpui::App) {
        self.read(cx).objects.clone().share_root(entity, cx);
    }

    fn share<S: Interface, T: Shared<S>>(&self, entity: &Entity<T>, cx: &mut gpui::App) -> Ref<S> {
        self.read(cx).objects.clone().share(entity, cx)
    }

    fn share_with<S: Interface, T: 'static>(
        &self,
        entity: &Entity<T>,
        register: impl FnOnce(&mut Methods<S, T>),
        cx: &mut gpui::App,
    ) -> Ref<S> {
        self.read(cx)
            .objects
            .clone()
            .share_with(entity, register, cx)
    }

    fn root<S: Interface>(&self, cx: &mut gpui::App) -> Remote<S> {
        self.read(cx).objects.clone().root()
    }

    fn connect<S: Interface>(&self, reference: Ref<S>, cx: &mut gpui::App) -> Remote<S> {
        self.read(cx).objects.clone().connect(reference)
    }

    fn view(&self, name: impl Into<String>, cx: &mut gpui::App) -> Entity<PluginViewState> {
        self.update(cx, |host, cx| host.view(name, cx))
    }

    fn pump(&self, cx: &mut gpui::App) {
        self.update(cx, |host, cx| host.pump(cx))
    }
}
