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

/// Статус спавненного демона: жив ли + хвост журнала.
#[tauri::command]
fn daemon_status(state: tauri::State<'_, DaemonHandle>) -> daemon::DaemonStatus {
    state.status()
}

fn main() {
    tauri::Builder::default()
        .manage(DaemonHandle::default())
        .invoke_handler(tauri::generate_handler![
            run_bottle,
            start_daemon,
            daemon_status
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно BottleForApps");
}
