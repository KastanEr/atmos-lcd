//! Tray-resident app: tray menu + Slint settings window around the worker.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

use crate::autostart;
use crate::config::Config;
use crate::render::SIZE;
use crate::worker::{Hooks, Status, Worker};

slint::include_modules!();

const SCREENS: [(&str, &str); 3] = [("system", "시스템 (CPU / GPU)"), ("clock", "시계"), ("image", "사진")];
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

/// Shared state on the UI thread.
struct App {
    config: Config,
    worker: Worker,
    window: AppWindow,
    tray: TrayMenu,
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
            if c.screen == "image" && c.image.is_none() {
                // Let the user pick a file in the window; a modal dialog here would
                // re-enter the event loop while `self` is borrowed.
                self.tray.sync(&self.config);
                self.window.set_screen_index(i as i32);
                self.window.set_status("사진을 선택하세요".into());
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
    let name = c.image.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
    w.set_image_name(name.unwrap_or_default().into());
}

fn show_window(w: &AppWindow) {
    let _ = w.show();
    w.window().set_minimized(false);
}

fn pick_image() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("LCD에 표시할 사진")
        .add_filter("이미지", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
        .pick_file()
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

    let app = Rc::new(RefCell::new(App { config, worker, window: window.clone_strong(), tray }));

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

    {
        let app = app.clone();
        window.on_settings_changed(move || {
            let c = app.borrow().from_window();
            app.borrow_mut().apply(c);
        });
    }
    {
        let app = app.clone();
        window.on_pick_image(move || {
            if let Some(path) = pick_image() {
                let c = Config { image: Some(path), screen: "image".into(), ..app.borrow().config.clone() };
                app.borrow_mut().apply(c);
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
