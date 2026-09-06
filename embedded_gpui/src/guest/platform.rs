use crate::dispatcher::PluginDispatcher;
use crate::surface::{Cursor, SurfaceApi};
use crate::text_system::PluginTextSystem;
use crate::window::{PluginWindow, PluginWindowState};
use anyhow::{Result, anyhow};
use embedded_gpui::Remote;
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, BackgroundExecutor, Bounds, ClipboardItem, CursorStyle,
    DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions, Pixels,
    Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Point, Task, ThermalState, WindowAppearance, WindowParams, px, size,
};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// The GPUI [`Platform`] implementation for Wasm plugin guests. There is no real display
/// here, and one window: the composition window every host surface is a root of (see
/// [`window`](crate::guest::window)). Rendering, text shaping, and scheduling are
/// delegated across the boundary.
pub struct PluginPlatform {
    dispatcher: Arc<PluginDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<PluginTextSystem>,
    display: Rc<PluginDisplay>,
    /// The composition window, once opened.
    window: RefCell<Option<Rc<PluginWindowState>>>,
    /// The surface that most recently received input: where cursor changes apply.
    last_input_surface: Rc<Cell<Option<u64>>>,
    /// A cursor style GPUI set since the last pump; flushed to `last_input_surface`.
    pending_cursor: Cell<Option<Cursor>>,
}

impl PluginPlatform {
    pub fn new() -> Self {
        let dispatcher = Arc::new(PluginDispatcher::new());
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        Self {
            dispatcher,
            background_executor,
            foreground_executor,
            text_system: Arc::new(PluginTextSystem::new()),
            display: Rc::new(PluginDisplay::new()),
            window: RefCell::new(None),
            last_input_surface: Rc::default(),
            pending_cursor: Cell::new(None),
        }
    }

    pub fn dispatcher(&self) -> &PluginDispatcher {
        &self.dispatcher
    }

    /// The composition window, if it is open. A state whose GPUI window has been
    /// removed is forgotten here.
    pub fn window(&self) -> Option<Rc<PluginWindowState>> {
        let mut window = self.window.borrow_mut();
        if window.as_ref().is_some_and(|window| window.is_closed()) {
            *window = None;
        }
        window.clone()
    }

    /// The cursor change GPUI requested since the last pump, and the surface it is for.
    pub fn take_pending_cursor(&self) -> Option<(Remote<SurfaceApi>, Cursor)> {
        let cursor = self.pending_cursor.take()?;
        let surface = self.last_input_surface.get()?;
        let remote = self.window()?.surface_remote(surface)?;
        Some((remote, cursor))
    }
}

impl Platform for PluginPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        // The host drives the run loop through the `tick` export; launching completes
        // synchronously. The caller must hold `Application::app_cell` to keep the app alive.
        on_finish_launching();
    }

    fn quit(&self) {}

    fn restart(&self, _binary_path: Option<PathBuf>, _args: Vec<OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        None
    }

    fn open_window(
        &self,
        _handle: AnyWindowHandle,
        _params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        if self.window().is_some() {
            return Err(anyhow!(
                "a plugin has one window; views are opened with embedded_gpui::open_view"
            ));
        }
        let state = Rc::new(PluginWindowState::new(
            self.text_system.clone(),
            self.last_input_surface.clone(),
        ));
        *self.window.borrow_mut() = Some(state.clone());
        Ok(Box::new(PluginWindow::new(state, self.display.clone())))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn open_url(&self, _url: &str) {}

    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("url schemes are not supported in plugins")))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Err(anyhow!("path prompts are not supported in plugins")))
            .ok();
        receiver
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Err(anyhow!("path prompts are not supported in plugins")))
            .ok();
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn compositor_name(&self) -> &'static str {
        "GpuiPlugin"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow!("app_path is not available in plugins"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow!(
            "auxiliary executables are not available in plugins"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        self.pending_cursor.set(Some(Cursor::from_gpui(style)));
    }

    fn hide_cursor_until_mouse_moves(&self) {}

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        None
    }

    fn write_to_clipboard(&self, _item: ClipboardItem) {}

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credentials are not available in plugins")))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credentials are not available in plugins")))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(PluginKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

struct PluginKeyboardLayout;

impl PlatformKeyboardLayout for PluginKeyboardLayout {
    fn id(&self) -> &str {
        "gpui-plugin"
    }

    fn name(&self) -> &str {
        "GPUI Plugin"
    }
}

#[derive(Debug)]
pub struct PluginDisplay {
    uuid: uuid::Uuid,
}

impl PluginDisplay {
    fn new() -> Self {
        Self {
            uuid: uuid::Uuid::from_u128(0x6770_7569_5f70_6c75_6769_6e00_0000_0001),
        }
    }
}

impl PlatformDisplay for PluginDisplay {
    fn id(&self) -> gpui::DisplayId {
        gpui::DisplayId::new(1)
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: size(px(8192.), px(8192.)),
        }
    }
}
