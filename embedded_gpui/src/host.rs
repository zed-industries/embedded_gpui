//! Host side: wasmtime glue, the request/turn transport, and the [`Surface`] entity that
//! replays guest display lists. See `DESIGN.md` for the architecture and
//! `wit/plugin.wit` for the wire protocol.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::clipboard::{ClipboardApi, ClipboardChanged};
use crate::registry::{Call, Frame, Objects, Response};
use crate::{Interface, Methods, Payload, Ref, Registry, Remote, Shared};
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, Context, Entity, PlatformTextSystem, Task, px};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
    });
}

use bindings::{Plugin, PluginImports};

mod surface;

pub use bindings::{
    InputAnswer, InputQuery, KeyDown as KeyDownQuery, Keystroke as WireKeystroke,
    Modifiers as WireModifiers, ReplaceAndMarkText, ReplaceText, TextForRange, TextRange,
    Utf16Selection,
};
pub use surface::Surface;

/// The data carried on the wasmtime `Store`: the WASI sandbox and the text system the
/// synchronous shaping imports answer from.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    text_system: Arc<dyn PlatformTextSystem>,
    limits: StoreLimits,
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
/// and `tick`, are the whole runtime surface. Every call runs under the turn budget:
/// a guest that exceeds it traps, and the instance is considered dead.
pub struct PluginInstance {
    store: Store<HostState>,
    bindings: Plugin,
    /// Epoch ticks a single guest turn may take before it traps.
    turn_deadline_ticks: u64,
    /// Epoch ticks an input query may take before it traps.
    query_deadline_ticks: u64,
    input_query_budget: Duration,
    limits: DisplayListLimits,
    /// Stops the epoch ticker thread when the instance drops.
    ticker_alive: Arc<AtomicBool>,
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        self.ticker_alive.store(false, Ordering::Relaxed);
    }
}

/// How often the engine's epoch advances; the granularity of the turn budget.
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Where [`PluginOptions::plugin_dir`] appears inside the guest.
const PLUGIN_DIR_GUEST_PATH: &str = "/plugin";

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
    /// The plugin's own directory, mounted read-only at `/plugin` in the guest: where a
    /// plugin keeps whatever it ships besides its component (assets, configuration,
    /// scripts). What is in it is the plugin's business; the host only grants access.
    pub plugin_dir: Option<std::path::PathBuf>,
    /// The most wall-clock time one guest turn (`init` or `tick`) may take. A guest
    /// that exceeds it traps and the instance stops; the host UI never waits on it
    /// either way, since turns run on a worker. Default: one second.
    pub turn_budget: Duration,
    /// The most the host UI thread waits for an input query (a keystroke's precedence,
    /// an IME question), and the most the guest may take answering one before it traps.
    /// Default: 50 milliseconds.
    pub input_query_budget: Duration,
    pub limits: DisplayListLimits,
    /// The most linear memory the guest may grow to. Default: 512 MiB.
    pub memory_limit: usize,
}

impl PluginOptions {
    pub fn new(text_system: Arc<dyn PlatformTextSystem>) -> Self {
        Self {
            text_system,
            configure_wasi: None,
            plugin_dir: None,
            turn_budget: Duration::from_secs(1),
            input_query_budget: Duration::from_millis(50),
            limits: DisplayListLimits::default(),
            memory_limit: 512 << 20,
        }
    }

    /// Mount `dir` read-only at `/plugin` in the guest. See [`PluginOptions::plugin_dir`].
    pub fn with_plugin_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.plugin_dir = Some(dir.into());
        self
    }

    pub fn with_turn_budget(mut self, budget: Duration) -> Self {
        self.turn_budget = budget;
        self
    }

    pub fn with_input_query_budget(mut self, budget: Duration) -> Self {
        self.input_query_budget = budget;
        self
    }

    pub fn with_limits(mut self, limits: DisplayListLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = bytes;
        self
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
        config.epoch_interruption(true);
        let engine = Engine::new(&config).context("creating wasmtime engine")?;

        // The epoch is the clock the turn budget is measured in. A thread per instance
        // is cheap; it exits when the instance drops.
        let ticker_alive = Arc::new(AtomicBool::new(true));
        std::thread::Builder::new()
            .name("embedded_gpui epoch".into())
            .spawn({
                let engine = engine.clone();
                let alive = ticker_alive.clone();
                move || {
                    while alive.load(Ordering::Relaxed) {
                        std::thread::sleep(EPOCH_TICK);
                        engine.increment_epoch();
                    }
                }
            })
            .context("spawning epoch ticker")?;

        let component = Component::from_file(&engine, component_path)
            .with_context(|| format!("loading component {}", component_path.display()))?;

        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).context("adding wasi to linker")?;
        Plugin::add_to_linker::<_, HostState>(&mut linker, |state| state)
            .context("adding plugin host imports to linker")?;

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.inherit_stdout().inherit_stderr();
        if let Some(dir) = &options.plugin_dir {
            wasi_builder
                .preopened_dir(dir, PLUGIN_DIR_GUEST_PATH, DirPerms::READ, FilePerms::READ)
                .with_context(|| format!("mounting plugin directory {}", dir.display()))?;
        }
        if let Some(configure) = options.configure_wasi {
            configure(&mut wasi_builder);
        }
        let wasi = wasi_builder.build();
        let state = HostState {
            wasi,
            table: ResourceTable::new(),
            text_system: options.text_system,
            limits: StoreLimitsBuilder::new()
                .memory_size(options.memory_limit)
                .build(),
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);
        let turn_deadline_ticks = options
            .turn_budget
            .as_millis()
            .div_ceil(EPOCH_TICK.as_millis())
            .max(1) as u64;
        store.set_epoch_deadline(turn_deadline_ticks);
        let bindings = Plugin::instantiate(&mut store, &component, &linker)
            .context("instantiating plugin component")?;

        Ok(Self {
            store,
            bindings,
            turn_deadline_ticks,
            query_deadline_ticks: budget_ticks(options.input_query_budget),
            input_query_budget: options.input_query_budget,
            limits: options.limits,
            ticker_alive,
        })
    }

    /// Run `Plugin::new` in the guest, then collect what it queued with an empty tick.
    pub fn init(&mut self) -> Result<bindings::Turn> {
        self.store.set_epoch_deadline(self.turn_deadline_ticks);
        self.bindings.call_init(&mut self.store)?;
        self.tick(Vec::new())
    }

    /// One guest turn: deliver `inbound`, run the guest's scheduler, collect its output.
    pub fn tick(&mut self, inbound: Vec<bindings::Frame>) -> Result<bindings::Turn> {
        self.store.set_epoch_deadline(self.turn_deadline_ticks);
        self.bindings.call_tick(&mut self.store, &inbound)
    }

    /// A synchronous text-input query, between turns, under the query budget.
    pub fn input_query(&mut self, view: u64, query: &InputQuery) -> Result<InputAnswer> {
        self.store.set_epoch_deadline(self.query_deadline_ticks);
        self.bindings.call_input_query(&mut self.store, view, query)
    }
}

/// How many epoch ticks a budget spans, rounded up, at least one.
fn budget_ticks(budget: Duration) -> u64 {
    (budget.as_millis() as u64)
        .div_ceil(EPOCH_TICK.as_millis() as u64)
        .max(1)
}

/// Caps on what a plugin may ship in one display list. A list over a cap stops the
/// plugin, like a trap: it is the one bulk path across the boundary, and a runaway
/// scene would otherwise cost the host every frame it replays it.
#[derive(Clone, Copy, Debug)]
pub struct DisplayListLimits {
    /// Primitives per display list (a surface's scene or overlay). Default: 100,000.
    pub max_primitives: usize,
    /// Bytes of new image payloads per display list. Default: 64 MiB.
    pub max_image_bytes: usize,
}

impl Default for DisplayListLimits {
    fn default() -> Self {
        Self {
            max_primitives: 100_000,
            max_image_bytes: 64 << 20,
        }
    }
}

impl DisplayListLimits {
    fn check(&self, list: &bindings::DisplayList) -> Result<(), String> {
        if list.primitives.len() > self.max_primitives {
            return Err(format!(
                "display list has {} primitives, more than the {} allowed",
                list.primitives.len(),
                self.max_primitives
            ));
        }
        let image_bytes: usize = list.new_images.iter().map(|image| image.bytes.len()).sum();
        if image_bytes > self.max_image_bytes {
            return Err(format!(
                "display list ships {image_bytes} bytes of images, more than the {} allowed",
                self.max_image_bytes
            ));
        }
        Ok(())
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
            promised: call.promised,
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
            promised: call.promised,
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
    /// A synchronous query the UI thread is waiting on (see [`InputQueries`]).
    Query {
        view: u64,
        query: InputQuery,
        reply: std::sync::mpsc::SyncSender<Result<InputAnswer, String>>,
    },
}

/// The host's synchronous channel into a plugin: the one place the UI thread calls the
/// guest and waits. An IME asks the focused text field questions mid-call, and key
/// precedence (did the guest consume this keystroke, or should it become text) must be
/// known before the host's own dispatch continues; neither can wait for a turn. The
/// wait is bounded by the turn budget: a guest that does not answer in time is treated
/// as having no text field, and a stopped guest answers nothing.
///
/// Installed as the plugin registry's extension, so a [`Surface`] finds it through the
/// view object it holds (`view.registry().extension::<InputQueries>()`).
pub struct InputQueries {
    requests: mpsc::UnboundedSender<PluginRequest>,
    timeout: Duration,
    clipboard: gpui::WeakEntity<Clipboard>,
}

/// The answer to a query sent with [`InputQueries::send`], arriving on the worker's
/// schedule.
pub struct PendingAnswer(std::sync::mpsc::Receiver<Result<InputAnswer, String>>);

impl PendingAnswer {
    /// The answer, if it has arrived.
    pub fn try_take(&self) -> Option<InputAnswer> {
        Self::unwrap(self.0.try_recv().ok())
    }

    fn wait(&self, timeout: Duration) -> Option<InputAnswer> {
        Self::unwrap(self.0.recv_timeout(timeout).ok())
    }

    fn unwrap(result: Option<Result<InputAnswer, String>>) -> Option<InputAnswer> {
        match result {
            Some(Ok(answer)) => Some(answer),
            Some(Err(error)) => {
                log::error!("embedded_gpui: input query failed: {error}");
                None
            }
            None => None,
        }
    }
}

impl InputQueries {
    /// Queue a query without waiting; the guest is ticked right after, so whatever the
    /// query changed gets rendered.
    pub fn send(&self, view: u64, query: InputQuery) -> PendingAnswer {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        if self
            .requests
            .unbounded_send(PluginRequest::Query { view, query, reply })
            .is_ok()
        {
            self.requests
                .unbounded_send(PluginRequest::Tick(Vec::new()))
                .ok();
        }
        PendingAnswer(answer)
    }

    /// Send a query and wait for its answer, at most the query budget.
    pub fn query(&self, view: u64, query: InputQuery) -> Option<InputAnswer> {
        let pending = self.send(view, query);
        let answer = pending.wait(self.timeout);
        if answer.is_none() {
            log::warn!("embedded_gpui: input query unanswered within the query budget");
        }
        answer
    }

    /// Let the host's clipboard object look at the clipboard again, ahead of a keystroke
    /// that may paste: a change reaches the plugin as an event before the key does.
    pub fn refresh_clipboard(&self, cx: &mut gpui::App) {
        self.clipboard
            .update(cx, |clipboard, cx| clipboard.refresh(cx))
            .ok();
    }
}

/// The host clipboard as an object (see [`crate::clipboard`]). Every [`PluginHost`]
/// owns one; hand its ref to a plugin through your root schema to grant the clipboard,
/// and don't to withhold it.
pub struct Clipboard {
    last_seen: Option<String>,
}

impl gpui::EventEmitter<ClipboardChanged> for Clipboard {}

impl Clipboard {
    /// Look at the clipboard; tell subscribers if it changed since the last look.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        if text != self.last_seen {
            self.last_seen = text.clone();
            cx.emit(ClipboardChanged { text });
        }
    }
}

#[crate::shared]
impl ClipboardApi for Clipboard {
    fn read(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        self.last_seen = text.clone();
        text
    }

    fn write(&mut self, text: String, cx: &mut Context<Self>) {
        self.last_seen = Some(text.clone());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }
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
    limits: DisplayListLimits,
    /// Why the plugin stopped, once it has: a trap, a budget, or a limit.
    stopped: Option<String>,
    clipboard: Entity<Clipboard>,
    _worker: Task<()>,
    _pump: Task<()>,
}

impl PluginHost {
    /// Move `instance` onto a background worker and wire the turn pump. A slow or
    /// misbehaving guest can never stall the UI thread: calls into wasm happen on the
    /// worker, strictly one at a time, and their output is applied back on the
    /// foreground in the same order.
    pub fn new(mut instance: PluginInstance, cx: &mut Context<Self>) -> Self {
        let input_query_budget = instance.input_query_budget;
        let limits = instance.limits;
        let clipboard = cx.new(|_| Clipboard { last_seen: None });
        let (requests, mut request_rx) = mpsc::unbounded::<PluginRequest>();
        let (turns_tx, mut turns_rx) = mpsc::unbounded::<Result<bindings::Turn, String>>();

        let worker = cx.background_spawn(async move {
            while let Some(request) = request_rx.next().await {
                let mut deferred = Vec::new();
                let turn = match request {
                    PluginRequest::Init => instance.init(),
                    PluginRequest::Tick(mut inbound) => {
                        // Everything already queued rides the same turn: one boundary
                        // crossing per burst, and the FIFO is preserved by construction.
                        // `Init` is always the first request, so nothing else can be
                        // queued behind a tick. A query queued behind the ticks runs
                        // after them, in order.
                        loop {
                            match request_rx.try_recv() {
                                Ok(PluginRequest::Tick(more)) => inbound.extend(more),
                                Ok(other) => {
                                    deferred.push(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        instance.tick(inbound)
                    }
                    PluginRequest::Query { view, query, reply } => {
                        let answer = instance
                            .input_query(view, &query)
                            .map_err(|error| format!("{error:#}"));
                        reply.send(answer).ok();
                        continue;
                    }
                };
                for request in deferred {
                    if let PluginRequest::Query { view, query, reply } = request {
                        let answer = instance
                            .input_query(view, &query)
                            .map_err(|error| format!("{error:#}"));
                        reply.send(answer).ok();
                    }
                }
                match turn {
                    Ok(turn) => {
                        if turns_tx.unbounded_send(Ok(turn)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // A trap (turn budget, memory limit, guest panic) leaves the
                        // guest in an unknown state: stop driving it, and tell the
                        // pump so in-flight calls fail instead of hanging.
                        let reason = format!("plugin stopped: {error:#}");
                        log::error!("embedded_gpui: {reason}");
                        turns_tx.unbounded_send(Err(reason)).ok();
                        break;
                    }
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
        objects.set_extension(Rc::new(InputQueries {
            requests: requests.clone(),
            timeout: input_query_budget,
            clipboard: clipboard.downgrade(),
        }));

        // Object traffic is applied straight to the registry, *outside* any update of
        // this entity: handlers run with the host entity un-borrowed, so user code in a
        // handler may freely use `PluginHostHandle` (e.g. a root method sharing a new
        // entity). Only the pixel path (scenes, wakeups) goes through the entity.
        let pump_objects = objects.clone();
        let pump = cx.spawn(async move |host, cx| {
            while let Some(turn) = turns_rx.next().await {
                let turn = match turn {
                    Ok(turn) => turn,
                    Err(reason) => {
                        cx.update(|cx| {
                            host.update(cx, |host, cx| host.stop(reason, cx)).ok();
                        });
                        break;
                    }
                };
                let applied = cx.update(|cx| {
                    pump_objects.drain_releases();
                    for frame in turn.frames {
                        pump_objects.deliver(frame_from_wire(frame), cx);
                    }
                    pump_objects.drain_releases();
                    host.update(cx, |host, cx| {
                        host.apply_scenes(turn.scenes, turn.overlays, cx);
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
            limits,
            stopped: None,
            clipboard,
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

    /// The host clipboard as an object. Share it with the plugin (through a method on
    /// your root, wrapped in a `Revocable` if the grant should be a loan) to give the
    /// plugin the clipboard; a plugin without the ref has none.
    pub fn clipboard(&self) -> Entity<Clipboard> {
        self.clipboard.clone()
    }

    /// Why the plugin stopped, if it has. Observers of this entity are notified when it
    /// does; every [`Surface`] it drew on shows the reason.
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// The plugin is done for good: a trap, a budget, a limit. In-flight calls fail,
    /// its surfaces show the reason, and the store is dropped.
    fn stop(&mut self, reason: String, cx: &mut Context<Self>) {
        if self.stopped.is_some() {
            return;
        }
        log::error!("embedded_gpui: {reason}");
        self.objects.fail_pending(&reason);
        for surface in self.objects.local_entities::<Surface>() {
            surface.update(cx, |surface, cx| surface.set_stopped(reason.clone(), cx));
        }
        self.stopped = Some(reason);
        self.scheduled_tick = None;
        self._worker = Task::ready(());
        cx.notify();
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
    fn apply_scenes(
        &mut self,
        scenes: Vec<bindings::Scene>,
        overlays: Vec<bindings::Scene>,
        cx: &mut Context<Self>,
    ) {
        for list in scenes.iter().chain(&overlays).map(|scene| &scene.list) {
            if let Err(reason) = self.limits.check(list) {
                self.stop(format!("plugin stopped: {reason}"), cx);
                return;
            }
        }
        for scene in scenes {
            self.ingest_images(&scene.list);
            match self.objects.local_entity::<Surface>(scene.surface) {
                Some(surface) => surface.update(cx, |surface, cx| {
                    surface.set_scene(scene.list, self.images.clone(), cx);
                }),
                None => log::warn!("embedded_gpui: scene for unknown surface {}", scene.surface),
            }
        }
        for overlay in overlays {
            self.ingest_images(&overlay.list);
            match self.objects.local_entity::<Surface>(overlay.surface) {
                Some(surface) => surface.update(cx, |surface, cx| {
                    surface.set_overlay(overlay.list, self.images.clone(), cx);
                }),
                None => log::warn!(
                    "embedded_gpui: overlay for unknown surface {}",
                    overlay.surface
                ),
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
