//! Точка входа GUI BottleForApps (Tauri 2, M4 Task 2).
//!
//! Rust-сторона минимальна по построению (рулинг D1): единственная
//! команда-посредник [`run_bottle`] спавнит CLI `bottle --json …`;
//! вся логика и рендер живут в TypeScript (`gui/ui`).

// Не открывать консольное окно рядом с GUI в release-сборке на Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bottle;

/// Команда-посредник для фронтенда: `bottle <args>` → stdout и код.
#[tauri::command]
async fn run_bottle(args: Vec<String>) -> Result<bottle::BottleOutput, String> {
    bottle::run_bottle(&args).await
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![run_bottle])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно BottleForApps");
}
