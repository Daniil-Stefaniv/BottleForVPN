//! Точка входа GUI BottleForApps (Tauri 2, M4).
//!
//! Rust-сторона минимальна по построению (рулинг D1): команда-посредник
//! [`run_bottle`] спавнит CLI `bottle --json …`, а [`start_daemon`]/
//! [`daemon_status`] — единственное место, где GUI запускает не-CLI
//! (кнопка «Запустить демона»); вся логика и рендер — в TypeScript.

// Не открывать консольное окно рядом с GUI в release-сборке на Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bottle;
mod daemon;
mod tray;

use daemon::DaemonHandle;

/// Команда-посредник для фронтенда: `bottle <args>` → stdout и код.
#[tauri::command]
async fn run_bottle(args: Vec<String>) -> Result<bottle::BottleOutput, String> {
    bottle::run_bottle(&args).await
}

/// Запускает демона bottled (кнопка «Запустить демона», рулинг D1).
#[tauri::command]
async fn start_daemon(state: tauri::State<'_, DaemonHandle>) -> Result<(), String> {
    state.start().await
}

/// Статус демона спавненного демона: жив ли + хвост журнала.
#[tauri::command]
fn daemon_status(state: tauri::State<'_, DaemonHandle>) -> daemon::DaemonStatus {
    state.status()
}

/// Обновляет tray-меню списком бутылок (вызывает TS после refresh).
#[tauri::command]
fn update_tray(
    app: tauri::AppHandle,
    state: tauri::State<'_, TrayState>,
    entries: Vec<tray::TrayEntry>,
) -> Result<(), String> {
    if !state.enabled {
        return Ok(());
    }
    tray::update_tray_menu(&app, entries).map_err(|error| error.to_string())
}

/// Включён ли tray (флаг `--no-tray` отключает: окно без tray,
/// закрытие = обычный выход).
#[tauri::command]
fn tray_enabled(state: tauri::State<'_, TrayState>) -> bool {
    state.enabled
}

/// Режим tray: создаётся в setup, отключается флагом `--no-tray`.
#[derive(Default)]
struct TrayState {
    enabled: bool,
}

fn main() {
    // Фича-флаг --no-tray: GUI без tray (закрытие окна = выход).
    let with_tray = !std::env::args().any(|arg| arg == "--no-tray");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(DaemonHandle::default())
        .manage(TrayState { enabled: with_tray })
        .invoke_handler(tauri::generate_handler![
            run_bottle,
            start_daemon,
            daemon_status,
            update_tray,
            tray_enabled
        ])
        .setup(move |app| {
            if with_tray {
                tray::create_tray(app.handle())
                    .map_err(|error| format!("не удалось создать tray-иконку: {error}"))?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно BottleForApps");
}
