//! Guest-side GPUI platform for the "GPUI embedded in GPUI" spike.
//!
//! A plugin runs a real GPUI [`App`] inside a `wasm32-wasip2` component. Each host
//! surface the plugin draws on becomes a GPUI window backed by [`window::PluginWindow`],
//! whose painted scenes are serialized into the turn instead of being sent to a GPU. See
//! `DESIGN.md`.

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
    Entity, Point, Render, SharedString, Window, WindowBounds, WindowHandle, WindowOptions,
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

/// Drain the guest scheduler and let dirty windows redraw, then report the next wakeup:
/// everything queued is drained before this returns, so only the earliest remaining timer
/// needs a host tick.
fn pump(platform: &PluginPlatform, async_app: &mut AsyncApp) -> Option<u32> {
    objects::drain_releases();
    let dispatcher = platform.dispatcher();
    for window in platform.window_states() {
        window.flush_events();
    }
    dispatcher.run_until_idle();
    for window in platform.window_states() {
        window.pump_frame();
    }
    dispatcher.run_until_idle();
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
/// One exists per window opened with [`open_view`]; it lives exactly as long as the host
/// holds it, and closes its window when released.
struct GuestView {
    window: Rc<window::PluginWindowState>,
    handle: AnyWindowHandle,
}

#[crate::shared]
impl ViewApi for GuestView {
    fn resize(&mut self, geometry: Geometry, _cx: &mut Context<Self>) {
        self.window.push_event(WindowEvent::Resize(
            size(px(geometry.width), px(geometry.height)),
            geometry.scale_factor,
        ));
    }

    fn mouse(&mut self, event: MouseEvent, _cx: &mut Context<Self>) {
        self.window
            .push_event(WindowEvent::Input(event.to_platform_input()));
    }

    fn key(&mut self, event: KeyEvent, _cx: &mut Context<Self>) {
        self.window
            .push_event(WindowEvent::Input(event.to_platform_input()));
    }
}

/// Open a GPUI window that draws on a host surface. `build` constructs the window's root
/// view exactly as with `cx.open_window`; the resulting view object is shared and
/// attached to the surface, after which the host drives its geometry and input.
///
/// The window lives as long as the host keeps the surface: when the host drops it, the
/// view is released and the window closes.
pub fn open_view<V: Render + 'static>(
    surface: Ref<SurfaceApi>,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> anyhow::Result<WindowHandle<V>> {
    let (_, platform) =
        runtime_handles().ok_or_else(|| anyhow::anyhow!("open_view before init"))?;
    let surface: Remote<SurfaceApi> = surface.connect();
    let surface_id = surface.reference().entity_id();
    platform.set_pending_surface(surface.clone());
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1.), px(1.)),
            })),
            ..Default::default()
        },
        build,
    )?;
    let window = platform
        .window(surface_id)
        .ok_or_else(|| anyhow::anyhow!("open_window did not bind the pending surface"))?;
    let view = cx.new(|cx| {
        let platform = platform.clone();
        cx.on_release(move |view: &mut GuestView, cx| {
            platform.forget_window(surface_id, &view.window);
            view.handle
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        })
        .detach();
        GuestView {
            window,
            handle: handle.into(),
        }
    });
    let view_ref = share(&view, cx);
    surface.attach(view_ref, cx);
    Ok(handle)
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
