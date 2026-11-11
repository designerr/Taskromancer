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

const BACKUPS_KEPT: usize = 14;
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

#[tauri::command]
fn show_main(app: AppHandle) {
    show_main_window(&app);
}

#[tauri::command]
fn hide_main(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn toggle_fullscreen(window: tauri::WebviewWindow) {
    let full = window.is_fullscreen().unwrap_or(false);
    let _ = window.set_fullscreen(!full);
}

fn open_mini_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("mini") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "mini", WebviewUrl::App("mini.html".into()))
        .title("Taskromancer mini")
        .inner_size(300.0, 96.0)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .build()
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
fn open_mini(app: AppHandle) -> Result<(), String> {
    open_mini_window(&app)
}

#[tauri::command]
fn close_mini(app: AppHandle) {
    if let Some(w) = app.get_webview_window("mini") {
        let _ = w.close();
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
                        // let the page save first; exit anyway if it does not answer
                        let _ = app.emit("tray-quit", ());
                        let handle = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(2500));
                            handle.exit(0);
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
            close_mini
        ])
        .run(tauri::generate_context!())
        .expect("error while running Taskromancer");
}
