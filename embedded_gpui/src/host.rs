//! Host side: wasmtime glue, the request/turn transport, and the [`Surface`] entity that
//! replays guest display lists. See `DESIGN.md` for the architecture and
//! `wit/plugin.wit` for the wire protocol.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::registry::{Call, Frame, Objects, Response};
use crate::{Interface, Methods, Payload, Ref, Registry, Remote, Shared};
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, Context, Entity, PlatformTextSystem, Task, px};
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

mod surface;

pub use surface::Surface;

/// The data carried on the wasmtime `Store`: the WASI sandbox and the text system the
/// synchronous shaping imports answer from.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    text_system: Arc<dyn PlatformTextSystem>,
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

/// A synchronous wasmtime store plus its instantiated bindings: the two exports, `init`
/// and `tick`, are the whole runtime surface.
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
        };
        let mut store = Store::new(&engine, state);
        let bindings = Plugin::instantiate(&mut store, &component, &linker)
            .context("instantiating plugin component")?;

        Ok(Self { store, bindings })
    }

    /// Run `Plugin::new` in the guest, then collect what it queued with an empty tick.
    pub fn init(&mut self) -> Result<bindings::Turn> {
        self.bindings.call_init(&mut self.store)?;
        self.tick(Vec::new())
    }

    /// One guest turn: deliver `inbound`, run the guest's scheduler, collect its output.
    pub fn tick(&mut self, inbound: Vec<bindings::Frame>) -> Result<bindings::Turn> {
        Ok(self.bindings.call_tick(&mut self.store, &inbound)?)
    }
}

/// Registry frames -> bindgen wire records, and back. Purely structural.
fn frame_to_wire(frame: Frame) -> bindings::Frame {
    match frame {
        Frame::Call(call) => bindings::Frame::Call(bindings::Call {
            target: call.target,
            request: call.request,
            method: call.method,
            payload: call.payload.bytes,
            refs: call.payload.refs,
        }),
        Frame::Response(response) => {
            let (outcome, refs) = match response.outcome {
                Ok(payload) => (Ok(payload.bytes), payload.refs),
                Err(error) => (Err(error), Vec::new()),
            };
            bindings::Frame::Response(bindings::Response {
                request: response.request,
                outcome,
                refs,
            })
        }
        Frame::Subscribe { target, observer } => {
            bindings::Frame::Subscribe(bindings::Subscribe { target, observer })
        }
        Frame::Release { target } => bindings::Frame::Release(bindings::Release { target }),
    }
}

fn frame_from_wire(frame: bindings::Frame) -> Frame {
    match frame {
        bindings::Frame::Call(call) => Frame::Call(Call {
            target: call.target,
            request: call.request,
            method: call.method,
            payload: Payload::from_parts(call.payload, call.refs),
        }),
        bindings::Frame::Response(response) => Frame::Response(Response {
            request: response.request,
            outcome: response
                .outcome
                .map(|bytes| Payload::from_parts(bytes, response.refs)),
        }),
        bindings::Frame::Subscribe(subscribe) => Frame::Subscribe {
            target: subscribe.target,
            observer: subscribe.observer,
        },
        bindings::Frame::Release(release) => Frame::Release {
            target: release.target,
        },
    }
}

/// Images shipped by the guest, cached per instance and shared by every surface it
/// draws on. Image ids are minted by the guest, so the cache is per instance.
pub type PluginImages = Rc<RefCell<HashMap<u64, Arc<gpui::RenderImage>>>>;

/// One call into the guest, queued for the background worker that owns the store. The
/// worker coalesces consecutive `Tick`s into one guest turn.
enum PluginRequest {
    Init,
    Tick(Vec<bindings::Frame>),
}

/// A GPUI entity that owns a plugin's wasmtime store (on a worker) and mediates between
/// the host application and the guest: its registry's transport, and the surfaces the
/// guest draws on.
pub struct PluginHost {
    /// Requests to the background worker that owns the wasmtime store. FIFO: the worker
    /// processes one turn at a time, and each turn's output comes back in order.
    requests: mpsc::UnboundedSender<PluginRequest>,
    images: PluginImages,
    /// This end's object registry: the object model lives there, side-blind; this
    /// entity supplies only its transport (the request queue) and the pixel path.
    objects: Objects,
    scheduled_tick: Option<Task<()>>,
    _worker: Task<()>,
    _pump: Task<()>,
}

impl PluginHost {
    /// Move `instance` onto a background worker and wire the turn pump. A slow or
    /// misbehaving guest can never stall the UI thread: calls into wasm happen on the
    /// worker, strictly one at a time, and their output is applied back on the
    /// foreground in the same order.
    pub fn new(mut instance: PluginInstance, cx: &mut Context<Self>) -> Self {
        let (requests, mut request_rx) = mpsc::unbounded::<PluginRequest>();
        let (turns_tx, mut turns_rx) = mpsc::unbounded::<bindings::Turn>();

        let worker = cx.background_spawn(async move {
            while let Some(request) = request_rx.next().await {
                let turn = match request {
                    PluginRequest::Init => instance.init(),
                    PluginRequest::Tick(mut inbound) => {
                        // Everything already queued rides the same turn: one boundary
                        // crossing per burst, and the FIFO is preserved by construction.
                        // `Init` is always the first request, so nothing else can be
                        // queued behind a tick.
                        while let Ok(PluginRequest::Tick(more)) = request_rx.try_recv() {
                            inbound.extend(more);
                        }
                        instance.tick(inbound)
                    }
                };
                match turn {
                    Ok(turn) => {
                        if turns_tx.unbounded_send(turn).is_err() {
                            break;
                        }
                    }
                    Err(error) => log::error!("embedded_gpui: plugin call failed: {error:#}"),
                }
            }
        });

        let sink = requests.clone();
        let objects = Objects::new(Box::new(move |frame| {
            let request = PluginRequest::Tick(vec![frame_to_wire(frame)]);
            if sink.unbounded_send(request).is_err() {
                log::error!("embedded_gpui: plugin worker is gone; dropping frame");
            }
        }));

        // Object traffic is applied straight to the registry, *outside* any update of
        // this entity: handlers run with the host entity un-borrowed, so user code in a
        // handler may freely use `PluginHostHandle` (e.g. a root method sharing a new
        // entity). Only the pixel path (scenes, wakeups) goes through the entity.
        let pump_objects = objects.clone();
        let pump = cx.spawn(async move |host, cx| {
            while let Some(turn) = turns_rx.next().await {
                let applied = cx.update(|cx| {
                    pump_objects.drain_releases();
                    for frame in turn.frames {
                        pump_objects.deliver(frame_from_wire(frame), cx);
                    }
                    pump_objects.drain_releases();
                    host.update(cx, |host, cx| {
                        host.apply_scenes(turn.scenes, cx);
                        host.schedule_wake(turn.wake_after_ms, cx);
                    })
                });
                if applied.is_err() {
                    break;
                }
            }
        });

        let this = Self {
            requests,
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
    /// a background thread, then hand back a ready [`PluginHost`]. Share your root,
    /// take theirs, and hand the plugin [`Surface`]s to draw on.
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

    /// This end's registry, for sharing and connecting from anywhere.
    pub fn registry(&self) -> Registry {
        Registry::new(self.objects.downgrade())
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

    /// Flush deferred work (queued capability releases) and give the guest a turn.
    /// Hosts with quiescent plugins (no pending tick) can call this to make drops
    /// observable.
    pub fn pump(&mut self, _cx: &mut Context<Self>) {
        self.objects.drain_releases();
        self.enqueue(PluginRequest::Tick(Vec::new()));
    }

    /// Route freshly rendered display lists to the surfaces they address. A scene names
    /// its surface by object id; the surface must be one this host shared, so a guest
    /// can only draw where it was handed a ref.
    fn apply_scenes(&mut self, scenes: Vec<bindings::Scene>, cx: &mut Context<Self>) {
        for scene in scenes {
            self.ingest_images(&scene.list);
            match self.objects.local_entity::<Surface>(scene.surface) {
                Some(surface) => surface.update(cx, |surface, cx| {
                    surface.set_scene(scene.list, self.images.clone(), cx);
                }),
                None => log::warn!("embedded_gpui: scene for unknown surface {}", scene.surface),
            }
        }
    }

    fn schedule_wake(&mut self, wake_after_ms: Option<u32>, cx: &mut Context<Self>) {
        if let Some(delay) = wake_after_ms {
            self.scheduled_tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(delay as u64))
                    .await;
                this.update(cx, |this, _| this.enqueue(PluginRequest::Tick(Vec::new())))
                    .ok();
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
    /// See [`PluginHost::registry`].
    fn registry(&self, cx: &gpui::App) -> Registry;

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

    /// See [`PluginHost::pump`].
    fn pump(&self, cx: &mut gpui::App);
}

impl PluginHostHandle for Entity<PluginHost> {
    fn registry(&self, cx: &gpui::App) -> Registry {
        self.read(cx).registry()
    }

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

    fn pump(&self, cx: &mut gpui::App) {
        self.update(cx, |host, cx| host.pump(cx))
    }
}
