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
use crate::slots;
use crate::worker::{Hooks, Status, StorageEvent, StorageOp, Worker};

slint::include_modules!();

const SCREENS: [(&str, &str); 3] = [("system", "시스템 (CPU / GPU)"), ("clock", "시계"), ("gallery", "미디어")];
const ROTATION_STEP: u16 = 60;

/// Tray menu items whose check marks mirror the config.
struct TrayMenu {
    open: MenuItem,
    offline: CheckMenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
    screens: Vec<CheckMenuItem>,
    rotations: Vec<CheckMenuItem>,
}

impl TrayMenu {
    fn build() -> (Menu, TrayMenu) {
        let open = MenuItem::new("설정 열기", true, None);
        let offline = CheckMenuItem::new("오프라인 모드 (쿨러 저장 화면)", true, false, None);
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
        menu.append(&offline).expect("append menu item");
        menu.append(&PredefinedMenuItem::separator()).expect("append menu item");
        menu.append(&autostart).expect("append menu item");
        menu.append(&quit).expect("append menu item");
        (menu, TrayMenu { open, offline, autostart, quit, screens, rotations })
    }

    fn sync(&self, config: &Config) {
        for (item, (key, _)) in self.screens.iter().zip(SCREENS) {
            item.set_checked(config.screen == key);
        }
        for (i, item) in self.rotations.iter().enumerate() {
            item.set_checked(config.rotation == i as u16 * ROTATION_STEP);
        }
        self.offline.set_checked(config.offline);
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
    /// Filled state of the cooler's stored-media slots, as last reported.
    device_slots: Vec<bool>,
    /// Slots being exported or uploaded, with the gallery item going in.
    storing: Vec<Option<Storing>>,
    /// Clears `notice` after a while.
    notice_timer: slint::Timer,
    /// Physical pixels per logical pixel the images were last prepared for.
    scale: f32,
    /// Latest unrotated preview frame (480x480 RGBA), kept to redo it at a new scale.
    last_frame: Option<Vec<u8>>,
}

/// A gallery item on its way into a storage slot.
struct Storing {
    id: String,
    name: String,
    /// `false` while converting, `true` while uploading.
    uploading: bool,
    progress: f32,
}

impl App {
    /// Saves, pushes to the worker and refreshes both UIs.
    fn apply(&mut self, config: Config) {
        if let Err(e) = config.save() {
            self.notify(format!("설정 저장 실패: {e}"));
        }
        self.worker.apply(config.clone());
        self.tray.sync(&config);
        load_into_window(&self.window, &config);
        self.config = config;
    }

    /// Shows the settings window and prepares images for its actual display scale.
    fn show(&mut self) {
        let _ = self.window.show();
        self.window.window().set_minimized(false);
        self.rescale();
        // The scale factor can settle only once the native window exists.
        self.notice_timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(200), || {
            with_app(|app| app.rescale());
        });
    }

    fn rescale(&mut self) {
        let scale = self.window.window().scale_factor();
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        self.window.set_preview_shadow(soft_shadow(scale));
        if let Some(frame) = self.last_frame.take() {
            self.set_preview_frame(frame);
        }
        self.refresh_gallery();
        self.refresh_slots();
    }

    fn set_preview_frame(&mut self, rgba: Vec<u8>) {
        if let Some(img) = image::RgbaImage::from_raw(SIZE, SIZE, rgba.clone()) {
            self.window.set_preview(display_image(&img, PREVIEW_SIZE, PREVIEW_RADIUS, self.scale));
        }
        self.last_frame = Some(rgba);
    }

    /// Shows a short message under the status badge for a few seconds.
    fn notify(&self, text: impl Into<String>) {
        self.window.set_notice(text.into().into());
        let weak = self.window.as_weak();
        self.notice_timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_secs(6), move || {
            if let Some(w) = weak.upgrade() {
                w.set_notice("".into());
            }
        });
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
                thumb: gallery::thumbnail_image(&item.id)
                    .map_or_else(Image::default, |img| display_image(&img, TILE_SIZE, TILE_RADIUS, self.scale)),
                badge: if item.is_animated() {
                    let secs = item.duration.as_secs_f32().round() as u64;
                    format!("{}:{:02}", secs / 60, secs % 60).into()
                } else {
                    "".into()
                },
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
            Err(e) => self.notify(format!("{name}: 변환 실패 ({e})")),
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
                    app.notify(format!("삭제 실패: {e}"));
                }
                app.refresh_gallery();
            });
        });
    }

    fn refresh_slots(&self) {
        let tiles: Vec<SlotTile> = (0..slots::COUNT)
            .map(|i| {
                let name = slots::name(i).unwrap_or_default();
                let thumb = if name.is_empty() { Image::default() } else { load_image(&slots::thumb_path(i), SLOT_SIZE, self.scale) };
                let job = self.storing[i].as_ref();
                SlotTile {
                    thumb,
                    name: name.into(),
                    filled: self.device_slots.get(i).copied().unwrap_or(false),
                    busy: job.is_some(),
                    phase: match job {
                        Some(j) if j.uploading => "업로드 중".into(),
                        Some(_) => "변환 중".into(),
                        None => "".into(),
                    },
                    progress: job.map_or(0.0, |j| j.progress),
                }
            })
            .collect();
        self.window.set_slots(ModelRc::new(VecModel::from(tiles)));
    }

    /// Exports gallery item `id` and uploads it into `slot`.
    fn store_slot(&mut self, slot: usize, id: String) {
        if self.storing[slot].is_some() {
            return;
        }
        let name = gallery::list().into_iter().find(|i| i.id == id).map(|i| i.name).unwrap_or_default();
        self.storing[slot] = Some(Storing { id: id.clone(), name, uploading: false, progress: 0.0 });
        self.refresh_slots();
        let rotation = self.config.split_rotation().1 as u16;
        std::thread::spawn(move || {
            let exported = gallery::export(&id, rotation, &gallery::export_dir(), |p| {
                with_app(move |app| app.store_progress(slot, false, p));
            });
            with_app(move |app| match exported {
                Ok(file) => app.worker.storage(StorageOp::Store(slot, file)),
                Err(e) => app.storage_event(StorageEvent::Failed(slot, e)),
            });
        });
    }

    fn store_progress(&mut self, slot: usize, uploading: bool, progress: f32) {
        if let Some(job) = self.storing[slot].as_mut() {
            job.uploading = uploading;
            job.progress = progress;
            self.refresh_slots();
        }
    }

    fn clear_slot(&mut self, slot: usize) {
        slots::forget(slot);
        self.worker.storage(StorageOp::Clear(slot));
    }

    fn toggle_offline(&mut self) {
        let c = Config { offline: !self.config.offline, ..self.config.clone() };
        self.apply(c);
    }

    fn toggle_display_in_sleep(&mut self) {
        let c = Config { display_in_sleep: !self.config.display_in_sleep, ..self.config.clone() };
        self.apply(c);
    }

    fn storage_event(&mut self, event: StorageEvent) {
        match event {
            StorageEvent::Slots(filled) => {
                // Forget records for slots emptied elsewhere (e.g. by MasterCTRL).
                for (i, f) in filled.iter().enumerate() {
                    if !f && self.storing.get(i).is_some_and(Option::is_none) {
                        slots::forget(i);
                    }
                }
                self.device_slots = filled;
            }
            StorageEvent::Uploading(slot, p) => return self.store_progress(slot, true, p),
            StorageEvent::Stored(slot) => {
                if let Some(Storing { id, name, .. }) = self.storing[slot].take() {
                    let _ = std::fs::remove_dir_all(gallery::export_dir().join(&id));
                    // The slot's thumbnail shows the result; only failures need words.
                    if let Err(e) = slots::record(slot, &id, &name) {
                        self.notify(format!("슬롯 기록 실패: {e}"));
                    }
                }
            }
            StorageEvent::Failed(slot, e) => {
                self.storing[slot] = None;
                self.notify(format!("슬롯 {} 저장 실패: {e}", slot + 1));
            }
        }
        self.refresh_slots();
    }

    /// Updates the registry and the tray check mark.
    fn set_autostart(&self, enabled: bool) {
        if let Err(e) = autostart::set_enabled(enabled) {
            self.notify(format!("자동 실행 설정 실패: {e}"));
        }
        let actual = autostart::is_enabled();
        self.tray.autostart.set_checked(actual);
    }

    fn handle_menu(&mut self, id: &MenuId) {
        if id == self.tray.open.id() {
            self.show();
        } else if id == self.tray.offline.id() {
            self.toggle_offline();
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
                self.notify("갤러리에서 항목을 선택하세요");
                self.show();
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
    w.set_offline(c.offline);
    w.set_display_in_sleep(c.display_in_sleep);
}

// Logical sizes and corner radii of the images in the window (see app.slint).
const PREVIEW_SIZE: f32 = 208.0;
const PREVIEW_RADIUS: f32 = 22.0;
const TILE_SIZE: f32 = 110.0;
const SLOT_SIZE: f32 = 164.0;
const TILE_RADIUS: f32 = 10.0;
/// The preview's shadow: blur radius, how far it drops, and its opacity.
const SHADOW_BLUR: f32 = 14.0;
const SHADOW_DROP: f32 = 8.0;
const SHADOW_ALPHA: f32 = 0.22;

/// Scales an image to exactly `logical` px at the window's `scale` and rounds its corners.
/// The software renderer scales with nearest-neighbour sampling (jagged) and cannot clip
/// to rounded corners, so both are done here.
fn display_image(img: &image::RgbaImage, logical: f32, radius: f32, scale: f32) -> Image {
    let px = (logical * scale).round().max(1.0) as u32;
    let sized = if img.dimensions() == (px, px) {
        img.clone()
    } else {
        image::imageops::resize(img, px, px, image::imageops::FilterType::CatmullRom)
    };
    Image::from_rgba8(rounded(sized.as_raw(), px, px, radius * scale))
}

fn load_image(path: &std::path::Path, logical: f32, scale: f32) -> Image {
    match image::open(path) {
        Ok(img) => display_image(&img.into_rgba8(), logical, TILE_RADIUS, scale),
        Err(_) => Image::default(),
    }
}

/// A blurred rounded rectangle for behind the preview, `2 * SHADOW_BLUR` larger on each side.
fn soft_shadow(scale: f32) -> Image {
    let margin = (2.0 * SHADOW_BLUR * scale).round() as u32;
    let size = (PREVIEW_SIZE * scale).round() as u32;
    let drop = (SHADOW_DROP * scale).round() as u32;
    let full = size + 2 * margin;
    let radius = PREVIEW_RADIUS * scale;
    let mut shape = image::RgbaImage::new(full, full);
    for y in 0..size.min(full - margin - drop) {
        for x in 0..size {
            let a = 255.0 * SHADOW_ALPHA * corner_coverage(x, y, size, size, radius);
            shape.put_pixel(x + margin, y + margin + drop, image::Rgba([0, 0, 0, a as u8]));
        }
    }
    let blurred = image::imageops::blur(&shape, SHADOW_BLUR * scale / 2.0);
    Image::from_rgba8(SharedPixelBuffer::clone_from_slice(blurred.as_raw(), full, full))
}

/// Share of pixel (x, y) inside a `width` x `height` rectangle with corners rounded by `radius`.
fn corner_coverage(x: u32, y: u32, width: u32, height: u32, radius: f32) -> f32 {
    let (w, h) = (width as f32, height as f32);
    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
    // Distance from the pixel center to the nearest corner circle's center, per axis.
    let dx = (radius - px).max(px - (w - radius)).max(0.0);
    let dy = (radius - py).max(py - (h - radius)).max(0.0);
    if dx == 0.0 || dy == 0.0 {
        return 1.0;
    }
    (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
}

/// Copies straight RGBA pixels with the corners outside `radius` made transparent
/// (antialiased). The software renderer cannot clip images to rounded corners.
fn rounded(rgba: &[u8], width: u32, height: u32, radius: f32) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, width, height);
    let pixels = buffer.make_mut_slice();
    for y in 0..height {
        for x in 0..width {
            let coverage = corner_coverage(x, y, width, height, radius);
            if coverage < 1.0 {
                let p = &mut pixels[(y * width + x) as usize];
                p.a = (p.a as f32 * coverage) as u8;
            }
        }
    }
    buffer
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
fn listen_for_show() {
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let name = wide(SHOW_EVENT);
    // Auto-reset; the handle lives as long as the process.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) } as usize;
    if event == 0 {
        return;
    }
    std::thread::spawn(move || {
        while unsafe { WaitForSingleObject(event as _, INFINITE) } == 0 {
            with_app(|app| app.show());
        }
    });
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
        let status_window = window.as_weak();
        Hooks {
            frame: Box::new(move |pixmap| {
                // Always store it: static screens (images) are drawn only once, possibly
                // while the window is hidden. A hidden window does not repaint, so this is cheap.
                let rgba = pixmap.data().to_vec();
                with_app(move |app| app.set_preview_frame(rgba));
            }),
            status: Box::new(move |status| {
                // Badge kinds: 0 connected, 1 searching, 2 busy, 3 error (see StatusBadge).
                let (kind, text, detail) = match status {
                    Status::Connected => (0, "연결됨", String::new()),
                    Status::Loading => (2, "불러오는 중", String::new()),
                    Status::NotFound => (1, "쿨러를 찾는 중", String::new()),
                    Status::Error(e) => (3, "오류", e),
                };
                let _ = status_window.upgrade_in_event_loop(move |w| {
                    w.set_status_kind(kind);
                    w.set_status(text.into());
                    w.set_notice(detail.into());
                });
            }),
            storage: Box::new(|event| with_app(move |app| app.storage_event(event))),
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

    let app = Rc::new(RefCell::new(App {
        config,
        worker,
        window: window.clone_strong(),
        tray,
        importer: Importer::spawn(),
        device_slots: vec![false; slots::COUNT],
        storing: (0..slots::COUNT).map(|_| None).collect(),
        notice_timer: slint::Timer::default(),
        // Unknown until the window is shown; `rescale` replaces it.
        scale: 0.0,
        last_frame: None,
    }));
    app.borrow_mut().rescale();
    app.borrow().refresh_slots();
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
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event {
            with_app(|app| app.show());
        }
    }));
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    listen_for_show();

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
    {
        let app = app.clone();
        window.on_drop_item(move |id, i| {
            let slot = i as usize;
            let filled = app.borrow().device_slots.get(slot) == Some(&true);
            // Replacing content is not undoable; ask first. No borrow is held while the dialog is open.
            if filled
                && rfd::MessageDialog::new()
                    .set_title("쿨러 저장공간")
                    .set_description(format!("슬롯 {}의 내용을 바꿀까요?", slot + 1))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    != rfd::MessageDialogResult::Yes
            {
                return;
            }
            app.borrow_mut().store_slot(slot, id.to_string());
        });
    }
    {
        let app = app.clone();
        window.on_clear_slot(move |i| app.borrow_mut().clear_slot(i as usize));
    }
    {
        let app = app.clone();
        window.on_toggle_offline(move || app.borrow_mut().toggle_offline());
    }
    {
        let app = app.clone();
        window.on_toggle_display_in_sleep(move || app.borrow_mut().toggle_display_in_sleep());
    }
    // Open the window on first run so a new user sees something; later starts stay in the tray.
    if !Config::exists() {
        app.borrow().config.save()?;
        app.borrow_mut().show();
    }
    // Closing the window only hides it; the app lives in the tray until "종료".
    slint::run_event_loop_until_quit()?;
    APP.with(|a| *a.borrow_mut() = None);
    Ok(())
}

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}
