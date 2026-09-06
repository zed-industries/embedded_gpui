//! Guest-side GPUI platform for the "GPUI embedded in GPUI" spike.
//!
//! A plugin runs a real GPUI [`App`] inside a `wasm32-wasip2` component, with one window:
//! a composition window backed by [`window::PluginWindow`]. Every host surface the plugin
//! draws on is a root attached to that window (`Window::attach_root`), and after each
//! frame the root's scene is read back on its own and serialized into the turn for its
//! surface, instead of being sent to a GPU. See `DESIGN.md`.

pub(crate) mod dispatcher;
mod objects;
pub(crate) mod platform;

pub use objects::{registry, root, share, share_root, share_with};
pub(crate) mod text_system;
pub(crate) mod window;

pub(crate) mod wit {
    #![allow(clippy::too_many_arguments)]

    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        skip: ["init"],
    });
}

use crate::surface::{Geometry, KeyEvent, MouseEvent, SurfaceApi, SurfaceApiCaller as _, ViewApi};
use crate::{Ref, Remote};
use gpui::{
    AnyWindowHandle, App, Application, ApplicationHandle, AssetSource, AsyncApp, Bounds, Context,
    Empty, Entity, IntoElement, Point, Render, SharedString, Window, WindowBounds, WindowOptions,
    prelude::*, px, size,
};
use platform::PluginPlatform;
use std::cell::RefCell;
use std::rc::Rc;
use window::WindowEvent;

/// A GPUI plugin. Implement this and call [`register_plugin!`] to make your crate a loadable
/// plugin component.
pub trait Plugin: 'static {
    /// Build the plugin's shared state. Runs once, when the host initializes the
    /// component. Construct your root object here and install it with [`share_root`]
    /// before returning; reach the host's root with [`root`]. Views are opened later,
    /// with [`open_view`], whenever the host hands you a surface.
    fn new(cx: &mut App) -> Self
    where
        Self: Sized;

    /// Assets (e.g. SVGs) bundled with the plugin, loadable by path from GPUI elements.
    fn assets() -> Option<Box<dyn AssetSource>>
    where
        Self: Sized,
    {
        None
    }
}

/// Registers a [`Plugin`] implementation as this component's entry point.
#[macro_export]
macro_rules! register_plugin {
    ($plugin_type:ty) => {
        #[unsafe(export_name = "init")]
        pub extern "C" fn __init_plugin() {
            $crate::initialize(<$plugin_type as $crate::Plugin>::assets(), |cx| {
                Box::new(<$plugin_type as $crate::Plugin>::new(cx))
            });
        }
    };
}

struct Runtime {
    // Keeps the guest App alive: PluginPlatform::run returns immediately, so unlike native
    // platforms nothing on the stack owns the app after launch.
    _app: ApplicationHandle,
    async_app: AsyncApp,
    platform: Rc<PluginPlatform>,
    _plugin: Box<dyn Plugin>,
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Delegating wrapper because `Box<dyn AssetSource>` itself does not implement the trait.
struct PluginAssets(Box<dyn AssetSource>);

impl AssetSource for PluginAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        self.0.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        self.0.list(path)
    }
}

#[doc(hidden)]
pub fn initialize(
    assets: Option<Box<dyn AssetSource>>,
    build_plugin: impl FnOnce(&mut App) -> Box<dyn Plugin> + 'static,
) {
    init_logger();

    let platform = Rc::new(PluginPlatform::new());
    let mut application = Application::with_platform(platform.clone());
    if let Some(assets) = assets {
        application = application.with_assets(PluginAssets(assets));
    }
    let platform_for_runtime = platform.clone();
    let plugin_slot: Rc<RefCell<Option<Box<dyn Plugin>>>> = Rc::default();
    let handle = application.run_embedded({
        let plugin_slot = plugin_slot.clone();
        move |cx| {
            *plugin_slot.borrow_mut() = Some(build_plugin(cx));
        }
    });
    let async_app = handle.to_async();
    let plugin = plugin_slot
        .borrow_mut()
        .take()
        .expect("Platform::run must invoke the launch callback synchronously");
    RUNTIME.with(|slot| {
        *slot.borrow_mut() = Some(Runtime {
            _app: handle,
            async_app,
            platform: platform_for_runtime,
            _plugin: plugin,
        });
    });
}

fn runtime_handles() -> Option<(AsyncApp, Rc<PluginPlatform>)> {
    RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|runtime| (runtime.async_app.clone(), runtime.platform.clone()))
    })
}

/// Drain the guest scheduler and let the window redraw, then report the next wakeup:
/// everything queued is drained before this returns, so only the earliest remaining timer
/// needs a host tick.
fn pump(platform: &PluginPlatform, async_app: &mut AsyncApp) -> Option<u32> {
    objects::drain_releases();
    let dispatcher = platform.dispatcher();
    let window = platform.window();
    if let Some(window) = &window {
        window.flush_events(async_app);
    }
    dispatcher.run_until_idle();
    if let Some(window) = &window {
        window.pump_frame();
    }
    dispatcher.run_until_idle();
    if let Some(window) = &window {
        async_app.update(|cx| window.ship_scenes(cx));
    }
    if let Some((surface, cursor)) = platform.take_pending_cursor() {
        async_app.update(|cx| {
            surface.set_cursor(cursor, cx);
        });
    }
    objects::drain_releases();
    dispatcher
        .next_timer_delay()
        .map(|delay| delay.as_millis().min(u32::MAX as u128) as u32)
}

/// The guest end of a host surface: the object the host drives with geometry and input.
/// One exists per view opened with [`open_view`]; it lives exactly as long as the host
/// holds it, and detaches its root when released.
struct GuestView {
    window: Rc<window::PluginWindowState>,
    surface: u64,
    generation: u64,
}

#[crate::shared]
impl ViewApi for GuestView {
    fn resize(&mut self, geometry: Geometry, _cx: &mut Context<Self>) {
        self.window.push_event(WindowEvent::Resize {
            surface: self.surface,
            size: size(px(geometry.width), px(geometry.height)),
            scale_factor: geometry.scale_factor,
        });
    }

    fn mouse(&mut self, event: MouseEvent, _cx: &mut Context<Self>) {
        self.window.push_event(WindowEvent::Input {
            surface: self.surface,
            input: event.to_platform_input(),
        });
    }

    fn key(&mut self, event: KeyEvent, _cx: &mut Context<Self>) {
        self.window.push_event(WindowEvent::Input {
            surface: self.surface,
            input: event.to_platform_input(),
        });
    }
}

/// The root view of the composition window. Nothing is drawn at the window level; every
/// visible thing is a surface's root.
struct CompositionRoot;

impl Render for CompositionRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Open the composition window if it is not open yet.
fn ensure_window(
    platform: &PluginPlatform,
    cx: &mut App,
) -> anyhow::Result<(Rc<window::PluginWindowState>, AnyWindowHandle)> {
    if let Some(window) = platform.window()
        && let Some(handle) = window.handle()
    {
        return Ok((window, handle));
    }
    let handle: AnyWindowHandle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: window::composition_size(),
                })),
                ..Default::default()
            },
            |_, cx| cx.new(|_| CompositionRoot),
        )?
        .into();
    let window = platform
        .window()
        .ok_or_else(|| anyhow::anyhow!("open_window did not create the composition window"))?;
    window.set_handle(handle);
    Ok((window, handle))
}

/// A view opened with [`open_view`]: the entity, and its place in the guest's window.
pub struct ViewHandle<V> {
    view: Entity<V>,
    window: Rc<window::PluginWindowState>,
    handle: AnyWindowHandle,
    surface: u64,
    generation: u64,
}

impl<V: 'static> ViewHandle<V> {
    pub fn entity(&self) -> &Entity<V> {
        &self.view
    }

    /// The window the view is a root of: the plugin's one composition window, which is
    /// where `open_view`'s builder ran.
    pub fn window(&self) -> AnyWindowHandle {
        self.handle
    }

    /// Update the view with its window in scope, as `WindowHandle::update` would.
    pub fn update<R>(
        &self,
        cx: &mut App,
        update: impl FnOnce(&mut V, &mut Window, &mut Context<V>) -> R,
    ) -> anyhow::Result<R> {
        let view = self.view.clone();
        self.handle.update(cx, |_, window, cx| {
            view.update(cx, |view, cx| update(view, window, cx))
        })
    }

    /// Stop drawing on the surface: the view's root is detached and the host's geometry
    /// and input for it are ignored from here on. The surface itself is untouched; the
    /// host may attach another view to it.
    pub fn close(&self, cx: &mut App) {
        self.window
            .remove_surface(self.surface, self.generation, cx);
    }
}

/// Draw on a host surface. `build` constructs a view in the plugin's window, exactly as a
/// window's root view is built with `cx.open_window`; the view becomes a root of that
/// window at the surface's slot (a node of its own, memoized like any mounted view), the
/// resulting view object is shared and attached to the surface, and the host then drives
/// its geometry and input.
///
/// The view lives as long as the host keeps the surface: when the host drops it, the
/// view object is released and its root is detached. [`ViewHandle::close`] ends it
/// early.
pub fn open_view<V: Render + 'static>(
    surface: Ref<SurfaceApi>,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> anyhow::Result<ViewHandle<V>> {
    let (_, platform) =
        runtime_handles().ok_or_else(|| anyhow::anyhow!("open_view before init"))?;
    let surface: Remote<SurfaceApi> = surface.connect();
    let surface_id = surface.reference().entity_id();
    let (window, handle) = ensure_window(&platform, cx)?;
    let view = handle.update(cx, |_, window, cx| build(window, cx))?;
    let generation = window.add_surface(surface.clone(), view.clone().into(), cx)?;
    let guest_view = cx.new(|cx| {
        cx.on_release(move |view: &mut GuestView, cx| {
            view.window
                .remove_surface(view.surface, view.generation, cx);
        })
        .detach();
        GuestView {
            window: window.clone(),
            surface: surface_id,
            generation,
        }
    });
    let view_ref = share(&guest_view, cx);
    surface.attach(view_ref, cx);
    Ok(ViewHandle {
        view,
        window,
        handle,
        surface: surface_id,
        generation,
    })
}

struct Component;

impl wit::Guest for Component {
    fn tick(inbound: Vec<wit::Frame>) -> wit::Turn {
        let Some((mut async_app, platform)) = runtime_handles() else {
            log::error!("embedded_gpui: tick before init");
            return objects::take_turn(None);
        };
        async_app.update(|cx| objects::deliver(inbound, cx));
        let wake_after_ms = pump(&platform, &mut async_app);
        objects::take_turn(wake_after_ms)
    }
}

wit::export!(Component with_types_in wit);

fn init_logger() {
    struct StderrLogger;

    impl log::Log for StderrLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            eprintln!("[plugin {}] {}", record.level(), record.args());
        }

        fn flush(&self) {}
    }

    static LOGGER: StderrLogger = StderrLogger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
