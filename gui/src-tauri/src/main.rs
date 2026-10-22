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

// ---------------------------------------------------------------------------
// Управление демоном и бутылками (главное окно, Task 15): список — из БД
// (работает без демона), старт/стоп/статистика — через DaemonClient.
// ---------------------------------------------------------------------------

/// Связь с демоном: event-цикл подписки спавнится один раз за сессию.
#[derive(Default)]
struct DaemonLink {
    loop_spawned: std::sync::atomic::AtomicBool,
}

/// Control-запрос демону свежим соединением (демон обслуживает
/// параллельные подключения; соединение закрывается после ответа).
async fn control_request(token: &str, req: bottle_core::ipc::GuiRequest) -> Result<bottle_core::ipc::GuiReply, String> {
    let client = daemon_client::DaemonClient::new()
        .ok_or_else(|| "каталог данных недоступен".to_string())?;
    client.connect(token).await?;
    client.request(req).await
}

/// Разблокированное хранилище из состояния (иначе понятная ошибка).
fn unlocked_vault<'a>(
    vault: &'a tauri::State<'a, VaultState>,
) -> Result<std::sync::MutexGuard<'a, Option<db::Vault>>, String> {
    let guard = vault.vault.lock().expect("vault не отравлен");
    if guard.is_none() {
        return Err("хранилище не разблокировано".to_string());
    }
    Ok(guard)
}

/// Список бутылок из БД (работает при выключенном демоне — спека §3).
#[tauri::command]
fn list_bottles(
    vault: tauri::State<'_, VaultState>,
) -> Result<Vec<db::bottles::BottleListRow>, String> {
    unlocked_vault(&vault)?
        .as_ref()
        .expect("проверено выше")
        .list_rows()
}

/// Подключается к демону: Hello → полная синхронизация профилей из БД →
/// один раз спавнится event-цикл (события → фронтенд). Вызывается после
/// разблокировки и когда watchdog увидел поднявшийся демон.
#[tauri::command]
async fn daemon_connect(
    app: tauri::AppHandle,
    daemon: tauri::State<'_, DaemonHandle>,
    link: tauri::State<'_, DaemonLink>,
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
) -> Result<(), String> {
    let profiles = {
        let guard = unlocked_vault(&vault)?;
        guard.as_ref().expect("проверено выше").all_full()?
    };
    let count = profiles.len();
    let token = daemon.token().to_string();
    let client = daemon_client::DaemonClient::new()
        .ok_or_else(|| "каталог данных недоступен".to_string())?;
    client.connect(&token).await?;
    client.sync_all(profiles).await?;
    let mut ctx = serde_json::Map::new();
    ctx.insert("count".to_string(), count.into());
    let _ = log.emit("profiles.synced", ctx);
    if !link
        .loop_spawned
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        client.spawn_event_loop(app, token);
    }
    Ok(())
}

/// Старт бутылки (креды демон уже имеет из синхронизации — спека §5).
#[tauri::command]
async fn start_bottle(
    daemon: tauri::State<'_, DaemonHandle>,
    log: tauri::State<'_, gui_log::GuiLog>,
    id: &str,
) -> Result<(), String> {
    let mut ctx = serde_json::Map::new();
    ctx.insert("id".to_string(), id.into());
    let _ = log.emit("bottle.start.requested", ctx);
    match control_request(
        daemon.token(),
        bottle_core::ipc::GuiRequest::StartBottle { id: id.to_string() },
    )
    .await?
    {
        bottle_core::ipc::GuiReply::Ok => Ok(()),
        bottle_core::ipc::GuiReply::Error { message } => Err(message),
        other => Err(format!("неожиданный ответ демона: {other:?}")),
    }
}

/// Стоп бутылки.
#[tauri::command]
async fn stop_bottle(
    daemon: tauri::State<'_, DaemonHandle>,
    log: tauri::State<'_, gui_log::GuiLog>,
    id: &str,
) -> Result<(), String> {
    let mut ctx = serde_json::Map::new();
    ctx.insert("id".to_string(), id.into());
    let _ = log.emit("bottle.stop.requested", ctx);
    match control_request(
        daemon.token(),
        bottle_core::ipc::GuiRequest::StopBottle { id: id.to_string() },
    )
    .await?
    {
        bottle_core::ipc::GuiReply::Ok => Ok(()),
        bottle_core::ipc::GuiReply::Error { message } => Err(message),
        other => Err(format!("неожиданный ответ демона: {other:?}")),
    }
}

/// «Остановить демона» (командная панель): Shutdown — стоп всех
/// бутылок и завершение процесса демона. GUI остаётся живым.
#[tauri::command]
async fn stop_daemon(
    daemon: tauri::State<'_, DaemonHandle>,
    log: tauri::State<'_, gui_log::GuiLog>,
) -> Result<(), String> {
    let _ = log.emit("daemon.shutdown", Default::default());
    // Демон гаснет после ответа — обрыв соединения вместо Ok не считаем
    // ошибкой: команда принята.
    match control_request(daemon.token(), bottle_core::ipc::GuiRequest::Shutdown).await {
        Ok(bottle_core::ipc::GuiReply::Error { message }) => Err(message),
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Статистика демона для командной панели и карт «Диагностики»;
/// Err — демон недоступен (watchdog помечает его выключенным).
#[tauri::command]
async fn daemon_stats(
    daemon: tauri::State<'_, DaemonHandle>,
) -> Result<bottle_core::ipc::DaemonStats, String> {
    match control_request(daemon.token(), bottle_core::ipc::GuiRequest::Stats).await? {
        bottle_core::ipc::GuiReply::Stats(stats) => Ok(*stats),
        bottle_core::ipc::GuiReply::Error { message } => Err(message),
        other => Err(format!("неожиданный ответ демона: {other:?}")),
    }
}

/// «Обновить логин и пароль» (пункт 9): сохранить в БД и точечно
/// синхронизировать профиль демону, если он запущен.
#[tauri::command]
async fn update_credentials(
    daemon: tauri::State<'_, DaemonHandle>,
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    id: &str,
    username: &str,
    password: &str,
) -> Result<(), String> {
    {
        let guard = unlocked_vault(&vault)?;
        guard
            .as_ref()
            .expect("проверено выше")
            .set_credentials(id, username, password)?;
    }
    let mut ctx = serde_json::Map::new();
    ctx.insert("id".to_string(), id.into());
    let _ = log.emit("bottle.credentials_updated", ctx);
    // Демон держит креды в памяти: обновляем точечно (не запущен — при
    // следующем daemon_connect уедет полная синхронизация).
    let full = {
        let guard = unlocked_vault(&vault)?;
        guard.as_ref().expect("проверено выше").get_full(id)?
    };
    if let Some(full) = full {
        let _ = control_request(
            daemon.token(),
            bottle_core::ipc::GuiRequest::UpsertProfile {
                profile: Box::new(full),
            },
        )
        .await;
    }
    Ok(())
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
        .manage(DaemonLink::default())
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
            legacy_import,
            list_bottles,
            daemon_connect,
            start_bottle,
            stop_bottle,
            stop_daemon,
            daemon_stats,
            update_credentials
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
