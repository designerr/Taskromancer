//! Desktop shell for Taskromancer: file storage with daily backups, tray, mini timer,
//! notifications, autostart, file dialogs and the random reference image.

use base64::Engine;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tauri_plugin_window_state::{AppHandleExt, StateFlags, WindowExt};

const BACKUPS_KEPT: usize = 14;

/// Remember size, position, monitor and maximised state, but never restore a window as hidden.
fn state_flags() -> StateFlags {
    StateFlags::all() & !StateFlags::VISIBLE
}
const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(err)?;
    fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads a text file as UTF-8, falling back to Windows-1251 (old Cyrillic .txt files).
fn read_any_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(err)?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes).to_vec();
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => {
            let raw = e.into_bytes();
            let (text, _, _) = encoding_rs::WINDOWS_1251.decode(&raw);
            Ok(text.into_owned())
        }
    }
}

// ---------- state on disk ----------

#[tauri::command]
fn load_state(app: AppHandle) -> Result<Option<String>, String> {
    let path = data_dir(&app)?.join("data.json");
    if path.exists() {
        Ok(Some(fs::read_to_string(path).map_err(err)?))
    } else {
        Ok(None)
    }
}

/// Writes the state atomically and keeps one backup per day for the last two weeks.
#[tauri::command]
fn save_state(app: AppHandle, json: String, day: String) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let tmp = dir.join("data.json.tmp");
    fs::write(&tmp, &json).map_err(err)?;
    fs::rename(&tmp, dir.join("data.json")).map_err(err)?;

    let backups = dir.join("backups");
    fs::create_dir_all(&backups).map_err(err)?;
    let safe_day: String = day.chars().filter(|c| c.is_ascii_digit() || *c == '-').collect();
    if !safe_day.is_empty() {
        fs::write(backups.join(format!("data-{safe_day}.json")), &json).map_err(err)?;
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&backups)
        .map_err(err)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();
    while files.len() > BACKUPS_KEPT {
        let old = files.remove(0);
        let _ = fs::remove_file(old);
    }
    Ok(())
}

#[tauri::command]
fn reveal_data(app: AppHandle) -> Result<(), String> {
    let dir = data_dir(&app)?;
    app.opener().open_path(dir.to_string_lossy().to_string(), None::<&str>).map_err(err)
}

// ---------- files ----------

#[tauri::command]
fn read_text(path: String) -> Result<(String, u64), String> {
    let p = PathBuf::from(&path);
    Ok((read_any_text(&p)?, mtime_ms(&p)))
}

#[tauri::command]
fn file_mtime(path: String) -> u64 {
    mtime_ms(Path::new(&path))
}

#[tauri::command]
async fn pick_txt_files(app: AppHandle) -> Vec<String> {
    app.dialog()
        .file()
        .add_filter("Text files", &["txt"])
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.to_string())
        .collect()
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog().file().blocking_pick_folder().map(|f| f.to_string())
}

/// Asks where to save, then writes the text. Returns false when the person cancels.
#[tauri::command]
async fn save_dialog(app: AppHandle, name: String, text: String) -> Result<bool, String> {
    let picked = app.dialog().file().set_file_name(&name).blocking_save_file();
    match picked.and_then(|f| f.into_path().ok()) {
        Some(path) => {
            fs::write(path, text).map_err(err)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Picks a random image from a folder and returns its path plus a data URL to show it.
#[tauri::command]
fn random_image(folder: String, avoid: Option<String>) -> Result<Option<(String, String)>, String> {
    let mut images: Vec<PathBuf> = fs::read_dir(&folder)
        .map_err(err)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| IMAGE_EXTS.contains(&x.to_lowercase().as_str()))
                .unwrap_or(false)
        })
        .collect();
    if images.is_empty() {
        return Ok(None);
    }
    if images.len() > 1 {
        if let Some(a) = avoid {
            images.retain(|p| p.to_string_lossy() != a);
        }
    }
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = &images[(seed % images.len() as u128) as usize];
    let bytes = fs::read(path).map_err(err)?;
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("png").to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/png",
    };
    let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes));
    Ok(Some((path.to_string_lossy().to_string(), url)))
}

#[tauri::command]
async fn pick_any_files(app: AppHandle) -> Vec<String> {
    app.dialog()
        .file()
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.to_string())
        .collect()
}

/// Which of these paths exist right now (used to grey out moved or deleted links).
#[tauri::command]
fn paths_exist(paths: Vec<String>) -> Vec<bool> {
    paths.iter().map(|p| Path::new(p).exists()).collect()
}

/// Names in a folder with a flag for sub-folders, used to turn a course folder into a checklist.
#[tauri::command]
fn list_dir(path: String) -> Result<Vec<(String, bool)>, String> {
    let mut out: Vec<(String, bool)> = fs::read_dir(&path)
        .map_err(err)?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path().is_dir()))
        .collect();
    out.sort();
    Ok(out)
}

#[tauri::command]
fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    app.opener().reveal_item_in_dir(path).map_err(err)
}

/// Returns (version, notes) when a newer signed release is published.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<(String, String)>, String> {
    let updater = app.updater().map_err(err)?;
    match updater.check().await.map_err(err)? {
        Some(u) => Ok(Some((u.version.clone(), u.body.clone().unwrap_or_default()))),
        None => Ok(None),
    }
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(err)?;
    if let Some(update) = updater.check().await.map_err(err)? {
        let _ = app.save_window_state(state_flags());
        update.download_and_install(|_, _| {}, || {}).await.map_err(err)?;
        app.restart();
    }
    Ok(())
}

/// Opens a web link (Jira etc.) in the default browser.
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only web links can be opened".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(err)
}

// ---------- boards: reference images live as files next to the data ----------

fn boards_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = data_dir(app)?.join("boards");
    fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

fn raw_body<'a>(request: &'a tauri::ipc::Request<'_>) -> Result<&'a [u8], String> {
    match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => Ok(bytes),
        _ => Err("expected raw bytes".into()),
    }
}

/// Header values arrive percent-encoded so that any file name survives the trip.
fn header(request: &tauri::ipc::Request<'_>, key: &str) -> Option<String> {
    let value = request.headers().get(key)?.to_str().ok()?;
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Downloads an image dragged in from a browser (the page itself can't, browsers block cross-site reads).
#[tauri::command]
async fn fetch_image(url: String) -> Result<tauri::ipc::Response, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only web addresses can be downloaded".into());
    }
    let origin = url.splitn(4, '/').take(3).collect::<Vec<_>>().join("/") + "/";
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(err)?;
    let resp = client
        .get(&url)
        .header("Referer", origin)
        .header("Accept", "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8,*/*;q=0.5")
        .send()
        .await
        .map_err(err)?
        .error_for_status()
        .map_err(err)?;
    let bytes = resp.bytes().await.map_err(err)?;
    if bytes.len() > 80 * 1024 * 1024 {
        return Err("that image is larger than 80 MB".into());
    }
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}

#[tauri::command]
fn board_dir(app: AppHandle) -> Result<String, String> {
    Ok(boards_dir(&app)?.to_string_lossy().to_string())
}

#[tauri::command]
fn save_board_image(app: AppHandle, request: tauri::ipc::Request<'_>) -> Result<String, String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let bytes = raw_body(&request)?;
    let ext: String = header(&request, "ext").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).take(5).collect();
    let ext = if ext.is_empty() { "png".to_string() } else { ext };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("img-{stamp}-{n}.{ext}");
    fs::write(boards_dir(&app)?.join(&name), bytes).map_err(err)?;
    Ok(name)
}

#[tauri::command]
fn read_board_image(app: AppHandle, name: String) -> Result<tauri::ipc::Response, String> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("bad file name".into());
    }
    let bytes = fs::read(boards_dir(&app)?.join(name)).map_err(err)?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Asks where to save, then writes the bytes (board exports). Returns false when cancelled.
#[tauri::command]
async fn save_bytes_dialog(app: AppHandle, request: tauri::ipc::Request<'_>) -> Result<bool, String> {
    let bytes = raw_body(&request)?.to_vec();
    let name = header(&request, "name").unwrap_or_else(|| "export.png".into());
    let picked = app.dialog().file().set_file_name(&name).blocking_save_file();
    match picked.and_then(|f| f.into_path().ok()) {
        Some(path) => {
            fs::write(path, bytes).map_err(err)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[tauri::command]
fn write_bytes(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let path = header(&request, "path").ok_or("missing path")?;
    fs::write(path, raw_body(&request)?).map_err(err)
}

#[tauri::command]
fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    app.opener().open_path(path, None::<&str>).map_err(err)
}

// ---------- system ----------

#[tauri::command]
fn notify(app: AppHandle, title: String, body: String) -> Result<(), String> {
    app.notification().builder().title(title).body(body).show().map_err(err)
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, on: bool) -> Result<bool, String> {
    let launcher = app.autolaunch();
    if on {
        launcher.enable().map_err(err)?;
    } else {
        launcher.disable().map_err(err)?;
    }
    launcher.is_enabled().map_err(err)
}

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn quit_now(app: &AppHandle) {
    let _ = app.save_window_state(state_flags());
    app.exit(0);
    // if anything keeps the loop alive, leave anyway
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        std::process::exit(0);
    });
}

#[tauri::command]
async fn show_main(app: AppHandle) {
    show_main_window(&app);
}

#[tauri::command]
async fn hide_main(app: AppHandle) {
    let _ = app.save_window_state(state_flags());
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
async fn quit_app(app: AppHandle) {
    quit_now(&app);
}

#[tauri::command]
async fn toggle_fullscreen(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let full = w.is_fullscreen().unwrap_or(false);
        let _ = w.set_fullscreen(!full);
    }
}

fn open_mini_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("mini") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }
    // first time: put it near the top-right corner of the main window, on the same monitor
    let (mut x, mut y) = (100.0, 100.0);
    if let Some(main) = app.get_webview_window("main") {
        if let (Ok(pos), Ok(size), Ok(scale)) = (main.outer_position(), main.outer_size(), main.scale_factor()) {
            x = (pos.x as f64 + size.width as f64) / scale - 340.0;
            y = pos.y as f64 / scale + 90.0;
        }
    }
    let w = WebviewWindowBuilder::new(app, "mini", WebviewUrl::App("mini.html".into()))
        .title("Taskromancer mini")
        .inner_size(300.0, 96.0)
        .position(x, y)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .disable_drag_drop_handler()
        .build()
        .map_err(err)?;
    let _ = w.restore_state(state_flags());
    Ok(())
}

#[tauri::command]
async fn open_mini(app: AppHandle) -> Result<(), String> {
    open_mini_window(&app)
}

#[tauri::command]
async fn close_mini(app: AppHandle) {
    let _ = app.save_window_state(state_flags());
    if let Some(w) = app.get_webview_window("mini") {
        let _ = w.close();
    }
}

#[tauri::command]
async fn set_fullscreen(app: AppHandle, on: bool) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_fullscreen(on);
    }
}

/// A board in its own borderless window that can stay on top of other programs, like PureRef.
#[tauri::command]
async fn open_board_window(app: AppHandle, id: String) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("board") {
        let _ = app.emit_to("board", "board-switch", id);
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }
    let script = format!("window.__BOARDWIN__ = {};", serde_json::to_string(&id).map_err(err)?);
    let w = WebviewWindowBuilder::new(&app, "board", WebviewUrl::App("index.html".into()))
        .title("Taskromancer board")
        .initialization_script(&script)
        .inner_size(720.0, 540.0)
        .min_inner_size(260.0, 200.0)
        .decorations(false)
        .transparent(true)
        .shadow(true)
        .always_on_top(true)
        .disable_drag_drop_handler()
        .build()
        .map_err(err)?;
    let _ = w.restore_state(state_flags());
    Ok(())
}

#[tauri::command]
async fn close_board_window(app: AppHandle) {
    let _ = app.save_window_state(state_flags());
    if let Some(w) = app.get_webview_window("board") {
        let _ = w.close();
    }
}

#[tauri::command]
async fn set_board_on_top(app: AppHandle, on: bool) {
    if let Some(w) = app.get_webview_window("board") {
        let _ = w.set_always_on_top(on);
    }
}

#[tauri::command]
async fn set_mini_on_top(app: AppHandle, on: bool) {
    if let Some(w) = app.get_webview_window("mini") {
        let _ = w.set_always_on_top(on);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--autostart"])))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().with_state_flags(state_flags()).build())
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Show Taskromancer", true, None::<&str>)?;
            let mini = MenuItem::with_id(app, "mini", "Mini timer", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &mini, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main-tray")
                .tooltip("Taskromancer")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "mini" => {
                        let _ = open_mini_window(app);
                    }
                    "quit" => {
                        // let the page save first; leave anyway if it does not answer
                        let _ = app.emit("tray-quit", ());
                        let handle = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(2500));
                            quit_now(&handle);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        show_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // started by Windows at login: open minimised instead of in the face
            if std::env::args().any(|a| a == "--autostart") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.minimize();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.emit("close-requested", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_state,
            save_state,
            reveal_data,
            read_text,
            file_mtime,
            pick_txt_files,
            pick_folder,
            save_dialog,
            random_image,
            open_path,
            notify,
            get_autostart,
            set_autostart,
            show_main,
            hide_main,
            quit_app,
            toggle_fullscreen,
            open_mini,
            close_mini,
            set_mini_on_top,
            pick_any_files,
            paths_exist,
            list_dir,
            reveal_path,
            check_update,
            install_update,
            open_url,
            board_dir,
            save_board_image,
            read_board_image,
            save_bytes_dialog,
            write_bytes,
            fetch_image,
            set_fullscreen,
            open_board_window,
            close_board_window,
            set_board_on_top
        ])
        .run(tauri::generate_context!())
        .expect("error while running Taskromancer");
}
