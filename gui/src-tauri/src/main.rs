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
mod daemon_client;
mod db;
mod gui_log;
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

/// Текст подтверждения выхода из трея («Завершить»): без активных
/// бутылок — короткий вопрос; с активными — перечисление id, они будут
/// остановлены вместе с демоном (0.3: «Завершить» гасит и GUI, и демона).
pub fn quit_confirm_text(active: &[String]) -> String {
    const QUESTION: &str = "Завершить Погребок и остановить демона?";
    if active.is_empty() {
        QUESTION.to_string()
    } else {
        format!("{QUESTION}\nАктивные бутылки будут остановлены: {}.", active.join(", "))
    }
}

/// Текст диалога подтверждения выхода (TS передаёт id активных бутылок).
#[tauri::command]
fn quit_confirm(active: Vec<String>) -> String {
    quit_confirm_text(&active)
}

/// Фронтенд логирует действие пользователя: код из реестра событий,
/// `src` всегда `gui`. Неизвестный код — ошибка (реестр един).
#[tauri::command]
fn log_event(
    state: tauri::State<'_, gui_log::GuiLog>,
    code: &str,
    ctx: serde_json::Value,
) -> Result<(), String> {
    let ctx = match ctx {
        serde_json::Value::Object(map) => map,
        _ => Default::default(),
    };
    state.emit(code, ctx)
}

/// Буфер журнала GUI для слияния лент в консоли «Диагностики».
#[tauri::command]
fn gui_log_recent(state: tauri::State<'_, gui_log::GuiLog>) -> Vec<bottle_core::events::Event> {
    state.recent()
}

/// Подтверждённый выход: шлёт демону `Shutdown` (стоп бутылок → выход)
/// и завершает GUI. Демон недоступен/не наш — просто выходим.
#[tauri::command]
async fn confirm_quit(
    app: tauri::AppHandle,
    state: tauri::State<'_, DaemonHandle>,
) -> Result<(), String> {
    const QUIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let token = state.token().to_string();
    let _ = tokio::time::timeout(QUIT_TIMEOUT, async {
        if let Some(client) = daemon_client::DaemonClient::new() {
            if client.connect(&token).await.is_ok() {
                let _ = client.request(bottle_core::ipc::GuiRequest::Shutdown).await;
            }
        }
    })
    .await;
    app.exit(0);
    Ok(())
}

fn main() {
    // Фича-флаг --no-tray: GUI без tray (закрытие окна = выход).
    let with_tray = !std::env::args().any(|arg| arg == "--no-tray");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Второй запуск: показываем и фокусируем окно первого
            // экземпляра (оно могло быть свёрнуто в трей).
            use tauri::Manager as _;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(DaemonHandle::default())
        .manage(TrayState { enabled: with_tray })
        // Журнал GUI: файл не открылся — работаем с буфером в памяти.
        .manage(
            gui_log::GuiLog::logs_dir()
                .and_then(|dir| gui_log::GuiLog::init(&dir).ok())
                .unwrap_or_else(gui_log::GuiLog::memory_only),
        )
        .invoke_handler(tauri::generate_handler![
            run_bottle,
            start_daemon,
            daemon_status,
            update_tray,
            tray_enabled,
            quit_confirm,
            confirm_quit,
            log_event,
            gui_log_recent
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Без активных бутылок — короткий вопрос (бриф Step 1).
    #[test]
    fn quit_confirm_without_active_is_short() {
        let text = quit_confirm_text(&[]);
        assert!(text.contains("Завершить Погребок"), "{text}");
        assert!(!text.contains("будут остановлены"), "{text}");
    }

    /// С активными — их id перечислены и предупреждены об остановке.
    #[test]
    fn quit_confirm_lists_active_bottles() {
        let text = quit_confirm_text(&["office-a".to_string(), "client-b".to_string()]);
        assert!(text.contains("Завершить Погребок"), "{text}");
        assert!(text.contains("office-a"), "{text}");
        assert!(text.contains("client-b"), "{text}");
        assert!(text.contains("будут остановлены"), "{text}");
    }
}
