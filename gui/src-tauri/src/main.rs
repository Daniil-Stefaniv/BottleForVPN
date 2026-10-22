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

// ---------------------------------------------------------------------------
// Хранилище (экран блокировки, Task 14): без пароля GUI не пускает дальше.
// ---------------------------------------------------------------------------

/// Состояние хранилища: разблокированный Vault и счётчик неудачных
/// попыток (для `db.unlock_failed {n}`).
#[derive(Default)]
struct VaultState {
    vault: std::sync::Mutex<Option<db::Vault>>,
    unlock_attempts: std::sync::Mutex<u32>,
}

/// Путь к файлу хранилища (`<data>/pogrebok.db`).
fn vault_path() -> Result<std::path::PathBuf, String> {
    bottle_core::ipc::default_data_dir()
        .map(|dir| dir.join(db::VAULT_FILE_NAME))
        .ok_or_else(|| "каталог данных недоступен".to_string())
}

/// Статус хранилища для экрана блокировки: файла нет — первый запуск;
/// есть — требуется пароль. `legacy_profiles` — число TOML-профилей
/// 0.2.x, доступных для импорта (0 — не предлагать миграцию).
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum VaultStatus {
    Absent { legacy_profiles: usize },
    Locked { legacy_profiles: usize },
}

#[tauri::command]
fn vault_status() -> Result<VaultStatus, String> {
    let legacy = db::migrate::detect_legacy().unwrap_or(0);
    if db::Vault::exists(&vault_path()?) {
        Ok(VaultStatus::Locked {
            legacy_profiles: legacy,
        })
    } else {
        Ok(VaultStatus::Absent {
            legacy_profiles: legacy,
        })
    }
}

/// Создаёт хранилище (первый запуск) и сразу «разблокирует» его.
#[tauri::command]
fn create_vault(
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    password: &str,
) -> Result<(), String> {
    let created = db::Vault::create(&vault_path()?, password)?;
    *vault.vault.lock().expect("vault не отравлен") = Some(created);
    let _ = log.emit("db.unlocked", Default::default());
    Ok(())
}

/// Разблокирует хранилище паролем. Возврат: `None` — успех,
/// `Some("wrong_password")` — неверный пароль (GUI показывает ошибку
/// под полем, не пуская дальше); повреждённая/занятая БД — Err.
#[tauri::command]
fn unlock_vault(
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    password: &str,
) -> Result<Option<String>, String> {
    match db::Vault::unlock(&vault_path()?, password) {
        Ok(opened) => {
            *vault.vault.lock().expect("vault не отравлен") = Some(opened);
            let _ = log.emit("db.unlocked", Default::default());
            Ok(None)
        }
        Err(db::UnlockError::WrongPassword) => {
            let mut attempts = vault.unlock_attempts.lock().expect("счётчик не отравлен");
            *attempts += 1;
            let mut ctx = serde_json::Map::new();
            ctx.insert("n".to_string(), (*attempts).into());
            let _ = log.emit("db.unlock_failed", ctx);
            Ok(Some("wrong_password".to_string()))
        }
        Err(db::UnlockError::Corrupt(detail)) => {
            let mut ctx = serde_json::Map::new();
            ctx.insert("detail".to_string(), detail.clone().into());
            let _ = log.emit("db.corrupt", ctx);
            Err(detail)
        }
        Err(db::UnlockError::Busy) => {
            Err("хранилище занято другим процессом — закройте второй Погребок".to_string())
        }
    }
}

/// Полный сброс хранилища (диалог «Забыли пароль?»): удаляет файл БД,
/// GUI возвращается на экран создания. Данные теряются безвозвратно.
#[tauri::command]
fn reset_vault(vault: tauri::State<'_, VaultState>) -> Result<(), String> {
    *vault.vault.lock().expect("vault не отравлен") = None; // закрыть БД до удаления
    db::migrate::reset_vault(&vault_path()?)
}

/// Отчёт об импорте профилей 0.2.x (кнопка «Импортировать»).
#[derive(serde::Serialize)]
struct ImportOutcome {
    imported: Vec<String>,
    skipped: Vec<(String, String)>,
}

/// Импортирует профили 0.2.x в разблокированное хранилище.
#[tauri::command]
fn legacy_import(vault: tauri::State<'_, VaultState>) -> Result<ImportOutcome, String> {
    let guard = vault.vault.lock().expect("vault не отравлен");
    let vault = guard
        .as_ref()
        .ok_or_else(|| "хранилище не разблокировано".to_string())?;
    let report = db::migrate::import_legacy(vault)?;
    Ok(ImportOutcome {
        imported: report.imported,
        skipped: report.skipped,
    })
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
        .manage(VaultState::default())
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
            gui_log_recent,
            vault_status,
            create_vault,
            unlock_vault,
            reset_vault,
            legacy_import
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
