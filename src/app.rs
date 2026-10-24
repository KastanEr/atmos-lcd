//! Tray-resident app: tray menu + Slint settings window around the worker.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};

use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

use crate::autostart;
use crate::config::Config;
use crate::gallery;
use crate::render::SIZE;
use crate::video;
use crate::worker::{Hooks, Status, Worker};

slint::include_modules!();

const SCREENS: [(&str, &str); 3] = [("system", "시스템 (CPU / GPU)"), ("clock", "시계"), ("gallery", "갤러리")];
const ROTATION_STEP: u16 = 60;

/// Tray menu items whose check marks mirror the config.
struct TrayMenu {
    open: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
    screens: Vec<CheckMenuItem>,
    rotations: Vec<CheckMenuItem>,
}

impl TrayMenu {
    fn build() -> (Menu, TrayMenu) {
        let open = MenuItem::new("설정 열기", true, None);
        let autostart = CheckMenuItem::new("Windows 시작 시 자동 실행", true, autostart::is_enabled(), None);
        let quit = MenuItem::new("종료", true, None);
        let screens: Vec<_> = SCREENS.iter().map(|(_, label)| CheckMenuItem::new(*label, true, false, None)).collect();
        let rotations: Vec<_> = (0..360 / ROTATION_STEP)
            .map(|i| CheckMenuItem::new(format!("{}°", i * ROTATION_STEP), true, false, None))
            .collect();
        let rotation_menu = Submenu::new("회전", true);
        for item in &rotations {
            rotation_menu.append(item).expect("append menu item");
        }
        let menu = Menu::new();
        menu.append(&open).expect("append menu item");
        menu.append(&PredefinedMenuItem::separator()).expect("append menu item");
        for item in &screens {
            menu.append(item).expect("append menu item");
        }
        menu.append(&rotation_menu).expect("append menu item");
        menu.append(&PredefinedMenuItem::separator()).expect("append menu item");
        menu.append(&autostart).expect("append menu item");
        menu.append(&quit).expect("append menu item");
        (menu, TrayMenu { open, autostart, quit, screens, rotations })
    }

    fn sync(&self, config: &Config) {
        for (item, (key, _)) in self.screens.iter().zip(SCREENS) {
            item.set_checked(config.screen == key);
        }
        for (i, item) in self.rotations.iter().enumerate() {
            item.set_checked(config.rotation == i as u16 * ROTATION_STEP);
        }
    }
}

/// Converts added files one at a time in the background.
struct Importer {
    tx: Sender<PathBuf>,
    /// Files waiting behind the one being converted.
    queued: Arc<AtomicUsize>,
}

impl Importer {
    fn spawn() -> Importer {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let queued = Arc::new(AtomicUsize::new(0));
        let waiting = queued.clone();
        std::thread::spawn(move || {
            for path in rx {
                let left = waiting.fetch_sub(1, Ordering::Relaxed) - 1;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let progress_name = name.clone();
                let result = gallery::import(&path, move |p| {
                    let more = if left > 0 { format!(" (대기 {left})") } else { String::new() };
                    let text = format!("변환 중: {progress_name} {:.0}%{more}", p * 100.0);
                    with_app(move |app| app.window.set_import_status(text.into()));
                });
                let done = waiting.load(Ordering::Relaxed) == 0;
                with_app(move |app| app.imported(&name, result, done));
            }
        });
        Importer { tx, queued }
    }

    fn add(&self, path: PathBuf) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(path);
    }
}

/// Runs `f` on the UI thread with the app state.
fn with_app(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        APP.with(|a| {
            if let Some(app) = a.borrow().as_ref() {
                f(&mut app.borrow_mut());
            }
        })
    });
}

/// Shared state on the UI thread.
struct App {
    config: Config,
    worker: Worker,
    window: AppWindow,
    tray: TrayMenu,
    importer: Importer,
}

impl App {
    /// Saves, pushes to the worker and refreshes both UIs.
    fn apply(&mut self, config: Config) {
        if let Err(e) = config.save() {
            self.window.set_status(format!("설정 저장 실패: {e}").into());
        }
        self.worker.apply(config.clone());
        self.tray.sync(&config);
        load_into_window(&self.window, &config);
        self.config = config;
    }

    /// Reloads the gallery grid from disk; forgets the selected item if it is gone.
    fn refresh_gallery(&mut self) {
        let items = gallery::list();
        if let Some(id) = &self.config.item
            && !items.iter().any(|i| &i.id == id)
        {
            let c = Config { item: None, ..self.config.clone() };
            self.apply(c);
        }
        let bytes: u64 = items.iter().map(|i| i.bytes).sum();
        let tiles: Vec<GalleryTile> = items
            .iter()
            .map(|item| GalleryTile {
                id: item.id.as_str().into(),
                name: item.name.as_str().into(),
                thumb: load_thumb(&item.id),
                badge: if item.is_animated() { format!("{:.0}s", item.duration.as_secs_f32()).into() } else { "".into() },
            })
            .collect();
        let summary = if items.is_empty() { String::new() } else { format!("{}개 · {:.0} MB", items.len(), bytes as f32 / 1e6) };
        self.window.set_gallery_summary(summary.into());
        self.window.set_tiles(ModelRc::new(VecModel::from(tiles)));
    }

    fn imported(&mut self, name: &str, result: Result<String, String>, queue_done: bool) {
        if queue_done {
            self.window.set_import_status("".into());
        }
        self.refresh_gallery();
        match result {
            // Show the first thing the user adds, so the effect is visible right away.
            Ok(id) if self.config.item.is_none() => {
                let c = Config { item: Some(id), screen: "gallery".into(), ..self.config.clone() };
                self.apply(c);
            }
            Ok(_) => {}
            Err(e) => self.window.set_status(format!("{name}: 변환 실패 ({e})").into()),
        }
    }

    fn select_item(&mut self, id: &str) {
        let c = Config { item: Some(id.to_string()), screen: "gallery".into(), ..self.config.clone() };
        self.apply(c);
    }

    fn delete_item(&mut self, id: &str) {
        if self.config.item.as_deref() == Some(id) {
            // Let the worker release the item's files before they are deleted.
            let c = Config { item: None, ..self.config.clone() };
            self.apply(c);
        }
        let id = id.to_string();
        std::thread::spawn(move || {
            // The worker may still be closing the file; retry briefly.
            let mut result = gallery::remove(&id);
            for _ in 0..10 {
                if result.is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                result = gallery::remove(&id);
            }
            with_app(move |app| {
                if let Err(e) = result {
                    app.window.set_status(format!("삭제 실패: {e}").into());
                }
                app.refresh_gallery();
            });
        });
    }

    /// Updates the registry and the tray check mark.
    fn set_autostart(&self, enabled: bool) {
        if let Err(e) = autostart::set_enabled(enabled) {
            self.window.set_status(format!("자동 실행 설정 실패: {e}").into());
        }
        let actual = autostart::is_enabled();
        self.tray.autostart.set_checked(actual);
    }

    fn handle_menu(&mut self, id: &MenuId) {
        if id == self.tray.open.id() {
            show_window(&self.window);
        } else if id == self.tray.autostart.id() {
            // The check mark already toggled itself; treat it as the requested state.
            self.set_autostart(self.tray.autostart.is_checked());
        } else if id == self.tray.quit.id() {
            let _ = slint::quit_event_loop();
        } else if let Some(i) = self.tray.screens.iter().position(|m| m.id() == id) {
            let mut c = self.config.clone();
            c.screen = SCREENS[i].0.into();
            if c.screen == "gallery" && c.item.is_none() {
                self.tray.sync(&self.config);
                self.window.set_screen_index(i as i32);
                self.window.set_status("갤러리에서 항목을 선택하세요".into());
                show_window(&self.window);
                return;
            }
            self.apply(c);
        } else if let Some(i) = self.tray.rotations.iter().position(|m| m.id() == id) {
            let c = Config { rotation: i as u16 * ROTATION_STEP, ..self.config.clone() };
            self.apply(c);
        }
    }

    /// Reads the window's controls into a new config.
    fn from_window(&self) -> Config {
        let w = &self.window;
        Config {
            screen: SCREENS[(w.get_screen_index().max(0) as usize).min(SCREENS.len() - 1)].0.into(),
            brightness: w.get_brightness().round().clamp(0.0, 100.0) as u8,
            rotation: w.get_rotation_index().max(0) as u16 * ROTATION_STEP,
            ..self.config.clone()
        }
    }
}

fn load_into_window(w: &AppWindow, c: &Config) {
    let screen = SCREENS.iter().position(|(k, _)| *k == c.screen).unwrap_or(0);
    w.set_screen_index(screen as i32);
    w.set_brightness(c.brightness as f32);
    w.set_rotation_index((c.rotation / ROTATION_STEP) as i32);
    w.set_selected_id(c.item.as_deref().unwrap_or("").into());
}

fn load_thumb(id: &str) -> Image {
    let Ok(img) = image::open(gallery::thumb_path(id)) else { return Image::default() };
    let img = img.into_rgba8();
    Image::from_rgba8(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

/// Named event a second launch sets to bring up the settings window.
const SHOW_EVENT: &str = "Local\\AtmosLcdShow";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn show_running_instance() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    let name = wide(SHOW_EVENT);
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if !h.is_null() {
            SetEvent(h);
            CloseHandle(h);
        }
    }
}

/// Shows the window whenever another launch signals `SHOW_EVENT`.
fn listen_for_show(window: slint::Weak<AppWindow>) {
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let name = wide(SHOW_EVENT);
    // Auto-reset; the handle lives as long as the process.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) } as usize;
    if event == 0 {
        return;
    }
    std::thread::spawn(move || {
        while unsafe { WaitForSingleObject(event as _, INFINITE) } == 0 {
            let _ = window.upgrade_in_event_loop(|w| show_window(&w));
        }
    });
}

fn show_window(w: &AppWindow) {
    let _ = w.show();
    w.window().set_minimized(false);
}

fn pick_files() -> Vec<PathBuf> {
    rfd::FileDialog::new()
        .set_title("갤러리에 추가할 사진 / GIF / 동영상")
        .add_filter("사진 / 동영상", &[&["png", "jpg", "jpeg", "gif", "webp", "bmp"][..], &video::EXTENSIONS].concat())
        .pick_files()
        .unwrap_or_default()
}

fn tray_icon_image() -> tray_icon::Icon {
    let img = image::load_from_memory(include_bytes!("../ui/icon.png")).expect("embedded icon").into_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("valid icon")
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load();
    let window = AppWindow::new()?;
    load_into_window(&window, &config);

    let hooks = {
        let frame_window = window.as_weak();
        let status_window = window.as_weak();
        Hooks {
            frame: Box::new(move |pixmap| {
                let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(pixmap.data(), SIZE, SIZE);
                // Always store it: static screens (images) are drawn only once, possibly
                // while the window is hidden. A hidden window does not repaint, so this is cheap.
                let _ = frame_window.upgrade_in_event_loop(move |w| w.set_preview(Image::from_rgba8(buffer)));
            }),
            status: Box::new(move |status| {
                let (connected, text) = match status {
                    Status::Connected => (true, "쿨러 연결됨".to_string()),
                    Status::Loading => (true, "불러오는 중…".to_string()),
                    Status::NotFound => (false, "쿨러를 찾는 중…".to_string()),
                    Status::Error(e) => (false, format!("오류: {e}")),
                };
                let _ = status_window.upgrade_in_event_loop(move |w| {
                    w.set_connected(connected);
                    w.set_status(text.into());
                });
            }),
        }
    };
    let worker = Worker::spawn(config.clone(), hooks);

    let (menu, tray) = TrayMenu::build();
    tray.sync(&config);
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Atmos LCD")
        .with_icon(tray_icon_image())
        .build()?;

    let app = Rc::new(RefCell::new(App { config, worker, window: window.clone_strong(), tray, importer: Importer::spawn() }));
    app.borrow_mut().refresh_gallery();

    // Tray events may arrive outside Slint's event loop; hop onto it.
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        let _ = slint::invoke_from_event_loop(move || {
            APP.with(|a| {
                if let Some(app) = a.borrow().as_ref() {
                    app.borrow_mut().handle_menu(&event.id);
                }
            })
        });
    }));
    let weak = window.as_weak();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event {
            let _ = weak.upgrade_in_event_loop(|w| show_window(&w));
        }
    }));
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    listen_for_show(window.as_weak());

    {
        let app = app.clone();
        window.on_settings_changed(move || {
            let c = app.borrow().from_window();
            app.borrow_mut().apply(c);
        });
    }
    {
        // The dialogs are modal; no borrow of `app` may be held while they are open.
        let app = app.clone();
        window.on_add_items(move || {
            for path in pick_files() {
                app.borrow().importer.add(path);
            }
        });
    }
    {
        let app = app.clone();
        window.on_select_item(move |id| app.borrow_mut().select_item(&id));
    }
    {
        let app = app.clone();
        window.on_delete_item(move |id| {
            let name = gallery::list().into_iter().find(|i| i.id == id.as_str()).map(|i| i.name).unwrap_or_default();
            let confirmed = rfd::MessageDialog::new()
                .set_title("갤러리에서 삭제")
                .set_description(format!("'{name}'을(를) 갤러리에서 삭제할까요? 원본 파일은 그대로 남아요."))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if confirmed == rfd::MessageDialogResult::Yes {
                app.borrow_mut().delete_item(&id);
            }
        });
    }
    // Open the window on first run so a new user sees something; later starts stay in the tray.
    if !Config::exists() {
        app.borrow().config.save()?;
        show_window(&window);
    }
    // Closing the window only hides it; the app lives in the tray until "종료".
    slint::run_event_loop_until_quit()?;
    APP.with(|a| *a.borrow_mut() = None);
    Ok(())
}

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}
