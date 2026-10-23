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

/// Папка журналов (gui-*.jsonl и bottled.log) — кнопка «Открыть папку
/// логов» в подвале консоли (спека §3.4).
#[tauri::command]
fn logs_dir() -> Result<String, String> {
    gui_log::GuiLog::logs_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .ok_or_else(|| "каталог данных недоступен".to_string())
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

// ---------------------------------------------------------------------------
// Окно редактора бутылки (Task 16, спека §3.3): дочернее окно ОС поверх
// заблокированного главного. Контекст (id + состояние) — через EditorState,
// данные и мутации — командами get_bottle/save_bottle/delete_bottle.
// ---------------------------------------------------------------------------

/// Контекст открытого редактора: какую бутылку править (`None` — создание)
/// и её последнее известное состояние (поднятая — баннер «изменения
/// применятся после перезапуска», удаление запрещено).
#[derive(Default, Clone, serde::Serialize)]
struct EditorContext {
    id: Option<String>,
    state: Option<String>,
}

#[derive(Default)]
struct EditorState {
    context: std::sync::Mutex<EditorContext>,
}

/// Данные бутылки для формы редактора (`get_bottle`).
#[derive(serde::Serialize)]
struct BottleEditorData {
    profile: bottle_core::profile::BottleProfile,
    /// Сохранены ли логин/пароль (форма пишет «уже в хранилище»).
    has_credentials: bool,
    /// Текст конфига уже в БД: edit без выбора нового файла сохранит его.
    has_config: bool,
}

/// Аргументы `save_bottle` из формы редактора (JS шлёт camelCase,
/// Tauri конвертирует в snake_case поля).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Deserialize)]
struct SaveBottleArgs {
    id: String,
    name: String,
    kind: bottle_core::profile::BottleKind,
    /// `true` — обновление существующей (id readonly), `false` —
    /// создание (дубль id — ошибка, Review Focus #5).
    is_edit: bool,
    // env:
    protocol: Option<bottle_core::profile::TunnelProtocol>,
    /// Файл конфига, выбранный пользователем; `None` при edit —
    /// оставить сохранённый в БД.
    config_path: Option<String>,
    // host:
    client: Option<String>,
    args: Option<String>,
    endpoints: Option<Vec<String>>,
    interface: Option<String>,
    // expose:
    socks5: u16,
    /// Цели пробросов `host:port` (порт слушателя назначит демон).
    forwards: Vec<String>,
    dns: Option<String>,
    launch: Option<String>,
    // Второй шаг OpenVPN («сохранить логин и пароль»):
    save_credentials: bool,
    username: Option<String>,
    password: Option<String>,
}

/// Собирает `FullProfile` из аргументов формы: валидация id, чтение
/// конфига (при edit без нового файла — прежний из БД), разбор
/// пробросов и DNS. Чистая функция — тесты без Tauri (бриф Step 1).
fn build_full_profile(
    args: &SaveBottleArgs,
    existing: Option<&bottle_core::ipc::FullProfile>,
    read_config: impl FnOnce(&str) -> Result<String, String>,
) -> Result<bottle_core::ipc::FullProfile, String> {
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec, VpnConfig};

    let id = args.id.trim();
    if id.is_empty() {
        return Err("идентификатор обязателен".to_string());
    }
    let valid_id = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !valid_id {
        return Err(
            "идентификатор: строчные латинские буквы, цифры и дефис; \
             начинается с буквы или цифры"
                .to_string(),
        );
    }
    let name = args.name.trim();
    if name.is_empty() {
        return Err("имя бутылки обязательно".to_string());
    }

    let mut read_config = Some(read_config);
    let (vpn, config_text) = match args.kind {
        BottleKind::Env => {
            let protocol = args
                .protocol
                .unwrap_or(bottle_core::profile::TunnelProtocol::WireGuard);
            // Правка без нового файла: имя и текст конфига — прежние.
            let previous = existing.and_then(|full| match &full.profile.vpn {
                VpnConfig::Env { config, .. } => {
                    Some((config.clone(), full.config_text.clone()))
                }
                VpnConfig::Host { .. } => None,
            });
            let (config_name, text) = match &args.config_path {
                Some(path) if !path.trim().is_empty() => {
                    let text = read_config.take().expect("read_config вызывается раз")(path)?;
                    // Имя файла — по обоим разделителям: приложение живёт
                    // на Windows (пути с `\`), тесты гоняются и на Unix.
                    let file_name = path
                        .rsplit(['/', '\\'])
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or(path)
                        .to_string();
                    (file_name, Some(text))
                }
                _ => previous.unwrap_or_default(),
            };
            let text =
                text.ok_or_else(|| "укажите файл конфигурации туннеля".to_string())?;
            (
                VpnConfig::Env {
                    protocol,
                    config: config_name,
                },
                Some(text),
            )
        }
        BottleKind::Host => {
            let client = args.client.clone().unwrap_or_default();
            if client.trim().is_empty() {
                return Err("укажите VPN-клиент (путь к исполняемому файлу)".to_string());
            }
            (
                VpnConfig::Host {
                    client: client.trim().to_string(),
                    args: args.args.clone().filter(|a| !a.trim().is_empty()),
                    endpoints: args
                        .endpoints
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|e| e.trim().to_string())
                        .filter(|e| !e.is_empty())
                        .collect(),
                    interface: args.interface.clone().filter(|i| !i.trim().is_empty()),
                },
                None,
            )
        }
    };

    let mut forwards = Vec::new();
    for target in &args.forwards {
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        let valid = target
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
        if !valid {
            return Err(format!("проброс «{target}»: ожидается формат host:port"));
        }
        forwards.push(ForwardSpec {
            listen: 0,
            target: target.to_string(),
        });
    }
    let dns = match &args.dns {
        Some(value) if !value.trim().is_empty() => Some(
            value
                .trim()
                .parse::<std::net::IpAddr>()
                .map_err(|_| format!("DNS «{value}»: ожидается IP-адрес"))?,
        ),
        _ => None,
    };
    let credentials = if args.save_credentials {
        let username = args.username.clone().unwrap_or_default();
        if username.trim().is_empty() {
            return Err("укажите логин для сохранения в хранилище".to_string());
        }
        Some(bottle_core::ipc::VpnCredentials {
            username: username.trim().to_string(),
            password: args.password.clone().unwrap_or_default(),
        })
    } else {
        None
    };
    Ok(bottle_core::ipc::FullProfile {
        profile: bottle_core::profile::BottleProfile {
            id: id.to_string(),
            name: name.to_string(),
            kind: args.kind,
            vpn,
            expose: ExposeConfig {
                socks5: args.socks5,
                forwards,
                dns,
                launch: args.launch.clone().filter(|l| !l.trim().is_empty()),
            },
        },
        config_text,
        credentials,
    })
}

/// Контекст для окна редактора (вызывается редактором при загрузке).
#[tauri::command]
fn editor_context(state: tauri::State<'_, EditorState>) -> EditorContext {
    state.context.lock().expect("editor не отравлен").clone()
}

/// Профиль бутылки для формы редактирования; `None` — нет такой.
#[tauri::command]
fn get_bottle(
    vault: tauri::State<'_, VaultState>,
    id: &str,
) -> Result<Option<BottleEditorData>, String> {
    let guard = unlocked_vault(&vault)?;
    let full = guard.as_ref().expect("проверено выше").get_full(id)?;
    Ok(full.map(|full| BottleEditorData {
        has_credentials: full.credentials.is_some(),
        has_config: full.config_text.is_some(),
        profile: full.profile,
    }))
}

/// «Создать»/«Сохранить» редактора: профиль в БД одной транзакцией,
/// точечная синхронизация демону (недоступен — доедет при connect),
/// событие `bottles-changed` главному окну.
#[tauri::command]
async fn save_bottle(
    app: tauri::AppHandle,
    daemon: tauri::State<'_, DaemonHandle>,
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    args: SaveBottleArgs,
) -> Result<(), String> {
    let existing = {
        let guard = unlocked_vault(&vault)?;
        guard.as_ref().expect("проверено выше").get_full(&args.id)?
    };
    if args.is_edit && existing.is_none() {
        return Err(format!("бутылка не найдена: {}", args.id));
    }
    let full = build_full_profile(&args, existing.as_ref(), |path| {
        std::fs::read_to_string(path)
            .map_err(|error| format!("не удалось прочитать файл конфигурации «{path}»: {error}"))
    })?;
    {
        let guard = unlocked_vault(&vault)?;
        let vault = guard.as_ref().expect("проверено выше");
        if args.is_edit {
            vault.update_bottle(&full)?;
        } else {
            vault.create_bottle(&full)?;
        }
    }
    let mut ctx = serde_json::Map::new();
    ctx.insert("id".to_string(), full.profile.id.clone().into());
    let _ = log.emit(
        if args.is_edit {
            "bottle.updated"
        } else {
            "bottle.created"
        },
        ctx,
    );
    let _ = control_request(
        daemon.token(),
        bottle_core::ipc::GuiRequest::UpsertProfile {
            profile: Box::new(full.clone()),
        },
    )
    .await;
    use tauri::Emitter as _;
    let _ = app.emit(
        "bottles-changed",
        serde_json::json!({
            "action": if args.is_edit { "updated" } else { "created" },
            "id": full.profile.id,
        }),
    );
    Ok(())
}

/// Удаление из окна редактирования (поднятую бутылку фронт не даёт
/// удалить — кнопка погашена): БД + `RemoveProfile` демону.
#[tauri::command]
async fn delete_bottle(
    app: tauri::AppHandle,
    daemon: tauri::State<'_, DaemonHandle>,
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    id: &str,
) -> Result<(), String> {
    {
        let guard = unlocked_vault(&vault)?;
        guard.as_ref().expect("проверено выше").delete_bottle(id)?;
    }
    let mut ctx = serde_json::Map::new();
    ctx.insert("id".to_string(), id.into());
    let _ = log.emit("bottle.deleted", ctx);
    let _ = control_request(
        daemon.token(),
        bottle_core::ipc::GuiRequest::RemoveProfile { id: id.to_string() },
    )
    .await;
    use tauri::Emitter as _;
    let _ = app.emit(
        "bottles-changed",
        serde_json::json!({ "action": "deleted", "id": id }),
    );
    Ok(())
}

/// Открывает редактор бутылки: дочернее окно ОС поверх главного;
/// главное блокируется (`set_enabled`) до закрытия редактора (спека
/// §3.3, п.8). Уже открыто — фокус на нём.
#[tauri::command]
fn open_editor(
    app: tauri::AppHandle,
    state: tauri::State<'_, EditorState>,
    id: Option<String>,
    bottle_state: Option<String>,
) -> Result<(), String> {
    use tauri::Manager as _;
    if let Some(window) = app.get_webview_window("editor") {
        let _ = window.set_focus();
        return Ok(());
    }
    *state.context.lock().expect("editor не отравлен") = EditorContext {
        id,
        state: bottle_state,
    };
    let parent = app
        .get_webview_window("main")
        .ok_or_else(|| "главное окно не найдено".to_string())?;
    let parent_for_close = parent.clone();
    let editor = tauri::WebviewWindowBuilder::new(
        &app,
        "editor",
        tauri::WebviewUrl::App("editor.html".into()),
    )
    .title("Погребок — бутылка")
    .inner_size(620.0, 700.0)
    .min_inner_size(480.0, 560.0)
    .always_on_top(true)
    .build()
    .map_err(|error| format!("не удалось открыть окно редактора: {error}"))?;
    parent
        .set_enabled(false)
        .map_err(|error| format!("не удалось заблокировать главное окно: {error}"))?;
    editor.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let _ = parent_for_close.set_enabled(true);
        }
    });
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

// ---------------------------------------------------------------------------
// Настройки (Task 18, спека §3.1/§9): смена мастер-пароля, meta-ключи ui.*,
// папка данных, восстановление размера окна.
// ---------------------------------------------------------------------------

/// Смена мастер-пароля: старый проверяется отдельным открытием хранилища
/// (неверный — `Some("wrong_password")`, без rekey), затем `PRAGMA rekey`
/// на живом соединении и событие `db.rekeyed` в журнал.
#[tauri::command]
fn change_password(
    vault: tauri::State<'_, VaultState>,
    log: tauri::State<'_, gui_log::GuiLog>,
    old_password: &str,
    new_password: &str,
) -> Result<Option<String>, String> {
    let guard = vault.vault.lock().expect("vault не отравлен");
    let opened = guard
        .as_ref()
        .ok_or_else(|| "хранилище не разблокировано".to_string())?;
    match db::Vault::unlock(&vault_path()?, old_password) {
        Ok(_) => {}
        Err(db::UnlockError::WrongPassword) => {
            return Ok(Some("wrong_password".to_string()));
        }
        Err(db::UnlockError::Corrupt(detail)) => return Err(detail),
        Err(db::UnlockError::Busy) => {
            return Err("хранилище занято — повторите попытку".to_string());
        }
    }
    opened.rekey(new_password)?;
    let _ = log.emit("db.rekeyed", Default::default());
    Ok(None)
}

/// Читает настройку из meta (`ui.tab`, `ui.win_w`, …). Отсутствующая — None.
#[tauri::command]
fn get_setting(
    vault: tauri::State<'_, VaultState>,
    key: &str,
) -> Result<Option<String>, String> {
    let guard = vault.vault.lock().expect("vault не отравлен");
    guard
        .as_ref()
        .ok_or_else(|| "хранилище не разблокировано".to_string())?
        .get_meta(key)
}

/// Пишет настройку в meta.
#[tauri::command]
fn set_setting(
    vault: tauri::State<'_, VaultState>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let guard = vault.vault.lock().expect("vault не отравлен");
    guard
        .as_ref()
        .ok_or_else(|| "хранилище не разблокировано".to_string())?
        .set_meta(key, value)
}

/// Папка данных приложения — показывается в настройках.
#[tauri::command]
fn data_dir() -> Result<String, String> {
    bottle_core::ipc::default_data_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .ok_or_else(|| "каталог данных недоступен".to_string())
}

/// Восстановление размера главного окна из настроек (ui.win_w/ui.win_h).
/// Значения зажаты в разумные рамки, чтобы мусор в БД не увёл окно
/// за экран.
#[tauri::command]
fn apply_window_size(
    app: tauri::AppHandle,
    width: f64,
    height: f64,
) -> Result<(), String> {
    use tauri::Manager as _;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "главное окно не найдено".to_string())?;
    let width = width.clamp(400.0, 10_000.0);
    let height = height.clamp(300.0, 10_000.0);
    window
        .set_size(tauri::Size::Logical(tauri::LogicalSize::new(width, height)))
        .map_err(|error| format!("не удалось изменить размер окна: {error}"))
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
        .manage(EditorState::default())
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
            logs_dir,
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
            update_credentials,
            editor_context,
            get_bottle,
            save_bottle,
            delete_bottle,
            open_editor,
            change_password,
            get_setting,
            set_setting,
            data_dir,
            apply_window_size
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

    // --- build_full_profile (Task 16, бриф Step 1) --------------------------

    use bottle_core::profile::{BottleKind, TunnelProtocol, VpnConfig};

    fn args_env() -> SaveBottleArgs {
        SaveBottleArgs {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            is_edit: false,
            protocol: Some(TunnelProtocol::WireGuard),
            config_path: Some(r"C:\vpn\wg-office.conf".to_string()),
            client: None,
            args: None,
            endpoints: None,
            interface: None,
            socks5: 0,
            forwards: vec!["10.20.30.15:3389".to_string()],
            dns: Some("10.20.30.1".to_string()),
            launch: Some("mstsc".to_string()),
            save_credentials: false,
            username: None,
            password: None,
        }
    }

    /// env-create: конфиг читается из выбранного файла, имя конфига —
    /// имя файла; пробросы и DNS разобраны.
    #[test]
    fn build_env_create_reads_config_file() {
        let full = build_full_profile(&args_env(), None, |path| {
            assert_eq!(path, r"C:\vpn\wg-office.conf");
            Ok("[Interface]\nPrivateKey = SECRET\n".to_string())
        })
        .unwrap();
        assert_eq!(full.profile.id, "office-a");
        assert_eq!(
            full.profile.vpn,
            VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: "wg-office.conf".to_string()
            }
        );
        assert_eq!(
            full.config_text.as_deref(),
            Some("[Interface]\nPrivateKey = SECRET\n")
        );
        assert_eq!(full.profile.expose.forwards.len(), 1);
        assert_eq!(full.profile.expose.forwards[0].listen, 0);
        assert_eq!(
            full.profile.expose.dns,
            Some("10.20.30.1".parse().unwrap())
        );
        assert!(full.credentials.is_none());
    }

    /// env-create без файла конфига — понятная ошибка.
    #[test]
    fn build_env_create_without_config_is_error() {
        let mut args = args_env();
        args.config_path = None;
        let error = build_full_profile(&args, None, |_| unreachable!()).unwrap_err();
        assert!(error.contains("файл конфигурации"), "{error}");
    }

    /// env-edit без нового файла: имя и текст конфига остаются прежними
    /// (пользователь не перевыбирал файл).
    #[test]
    fn build_env_edit_keeps_stored_config() {
        let existing = build_full_profile(&args_env(), None, |_| {
            Ok("OLD CONFIG".to_string())
        })
        .unwrap();
        let mut args = args_env();
        args.is_edit = true;
        args.config_path = None;
        args.name = "Переименованная".to_string();
        let full = build_full_profile(&args, Some(&existing), |_| {
            panic!("файл не читается, если не перевыбрали")
        })
        .unwrap();
        assert_eq!(full.config_text.as_deref(), Some("OLD CONFIG"));
        assert_eq!(
            full.profile.vpn,
            VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: "wg-office.conf".to_string()
            }
        );
        assert_eq!(full.profile.name, "Переименованная");
    }

    /// host-create: клиент обязателен; конфиг не пишется (даже если
    /// бутылка правится из env — призрака не остаётся).
    #[test]
    fn build_host_requires_client_and_drops_config() {
        let mut args = args_env();
        args.kind = BottleKind::Host;
        args.config_path = None;
        let error = build_full_profile(&args, None, |_| unreachable!()).unwrap_err();
        assert!(error.contains("VPN-клиент"), "{error}");

        args.client = Some(r"C:\vpn\cisco.exe".to_string());
        args.endpoints = Some(vec![" vpn.example.com ".to_string(), String::new()]);
        let full = build_full_profile(&args, None, |_| unreachable!()).unwrap();
        assert_eq!(
            full.profile.vpn,
            VpnConfig::Host {
                client: r"C:\vpn\cisco.exe".to_string(),
                args: None,
                endpoints: vec!["vpn.example.com".to_string()],
                interface: None,
            }
        );
        assert!(full.config_text.is_none());
    }

    /// Кривой проброс и кривой DNS — ошибки с указанием значения.
    #[test]
    fn build_rejects_bad_forward_and_dns() {
        let mut args = args_env();
        args.forwards = vec!["без-порта".to_string()];
        let error = build_full_profile(&args, None, |_| Ok("x".to_string())).unwrap_err();
        assert!(error.contains("host:port"), "{error}");

        let mut args = args_env();
        args.dns = Some("не-ip".to_string());
        let error = build_full_profile(&args, None, |_| Ok("x".to_string())).unwrap_err();
        assert!(error.contains("DNS"), "{error}");
    }

    /// Чекбокс «сохранить логин и пароль» без логина — ошибка;
    /// с логином — креды в профиль (демон получит в память).
    #[test]
    fn build_credentials_step() {
        let mut args = args_env();
        args.save_credentials = true;
        let error = build_full_profile(&args, None, |_| Ok("x".to_string())).unwrap_err();
        assert!(error.contains("логин"), "{error}");

        args.username = Some(" vpn-user ".to_string());
        args.password = Some("s3cret".to_string());
        let full = build_full_profile(&args, None, |_| Ok("x".to_string())).unwrap();
        let creds = full.credentials.expect("креды собраны");
        assert_eq!(creds.username, "vpn-user");
        assert_eq!(creds.password, "s3cret");
    }

    /// Невалидный id — ошибка до чтения файла (дешёвая проверка).
    #[test]
    fn build_rejects_invalid_id() {
        let mut args = args_env();
        args.id = "Офис А".to_string();
        let error = build_full_profile(&args, None, |_| unreachable!()).unwrap_err();
        assert!(error.contains("идентификатор"), "{error}");
    }
}
