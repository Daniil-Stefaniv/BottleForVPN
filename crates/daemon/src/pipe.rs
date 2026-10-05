//! Канал управления демона: диспетчер команд CLI и named-pipe транспорт.
//!
//! [`dispatch`] — чистая функция над хранилищем, исполняется на любой
//! платформе и покрывается unit-тестами. Транспорт (Windows named pipe
//! `\\.\pipe\bottled`, [`serve`]) обслуживает клиентов параллельно:
//! принятое соединение уходит в свою задачу `tokio::spawn`, а цикл
//! сразу пересоздаёт экземпляр канала для следующих подключений.
//! Одновременно обслуживается столько клиентов, сколько живых
//! экземпляров канала; долгий `start` одного клиента не блокирует
//! ответы остальным (T13).

use bottle_core::ipc::{BottleState, BottleSummary, CliReply, CliRequest};
use bottle_core::profile::BottleProfile;

use crate::appliance::WslRunner;
use crate::error::DaemonError;
use crate::manager::BottleManager;
use crate::store::Store;

#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use bottle_core::frame::LengthJsonCodec;
#[cfg(windows)]
use bytes::BytesMut;
#[cfg(windows)]
use tokio::io::{split, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
#[cfg(windows)]
use tokio::task::JoinSet;
#[cfg(windows)]
use tokio_stream::StreamExt;
#[cfg(windows)]
use tokio_util::codec::{Encoder, FramedRead};
#[cfg(windows)]
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use crate::Daemon;

/// Переходный ответ на команды управления, когда менеджер бутылок не
/// подключён (только тесты и dev-сборки без аплаенса).
const MANAGER_NOT_WIRED: &str = "менеджер бутылок не подключён";

/// Диспетчер команд CLI: `List`/`Show` читают хранилище `store`,
/// `Start`/`Stop`/`Status` и рантайм `Show` обслуживает менеджер бутылок
/// (если подключён).
///
/// Побочные эффекты — только чтение хранилища, вызовы менеджера и
/// warn-логи; битые профили в `List` пропускаются с warn-логом. `Show`
/// на отсутствующем/битом профиле отвечает `profile: None` (клиент
/// показывает «не найден»); рантайм берётся из `manager.status`.
// Вызывается windows-транспортом; на остальных платформах — только тестами.
#[cfg_attr(not(windows), allow(dead_code))]
pub async fn dispatch(
    store: &Store,
    manager: Option<&BottleManager>,
    wsl: Option<&dyn WslRunner>,
    rootfs: Option<&std::path::Path>,
    req: CliRequest,
) -> CliReply {
    match req {
        CliRequest::List => CliReply::List {
            bottles: list_summaries(store),
        },
        CliRequest::Show { id } => CliReply::Show {
            profile: show_profile(store, &id),
            runtime: match manager {
                Some(manager) => manager.status().await.into_iter().find(|rt| rt.id == id),
                None => None,
            },
        },
        CliRequest::Start { id } => match manager {
            Some(manager) => manager
                .start(&id)
                .await
                .unwrap_or_else(|error| CliReply::Error {
                    message: error.to_string(),
                }),
            None => CliReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        CliRequest::Stop { id } => match manager {
            Some(manager) => manager
                .stop(&id)
                .await
                .unwrap_or_else(|error| CliReply::Error {
                    message: error.to_string(),
                }),
            None => CliReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        CliRequest::Status => match manager {
            Some(manager) => CliReply::Status {
                bottles: manager.status().await,
            },
            None => CliReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        CliRequest::Create { profile, force } => create_bottle(store, &profile, force).await,
        CliRequest::Delete { id } => {
            // Активную бутылку удалять нельзя (start держит слушатели
            // и сессию аплаенса); без менеджера активных нет.
            // Активна = держит слушатели/сессию (Starting/Up/Degraded/
            // Stopping). Failed — не активная: namespace разобран при
            // неудаче старта, а stop на Failed — no-op; требовать
            // остановки означало бы неудаляемую бутылку (finding 1
            // финального ревью M3).
            let active = match manager {
                Some(manager) => manager
                    .status()
                    .await
                    .into_iter()
                    .any(|state| state.id == id && is_active_state(&state.state)),
                None => false,
            };
            if active {
                return CliReply::Error {
                    message: format!(
                        "бутылка {id} активна — остановите её перед удалением (bottle stop {id})"
                    ),
                };
            }
            match store.delete(&id) {
                Ok(conf_existed) => CliReply::Deleted {
                    id,
                    warning: (!conf_existed).then(|| {
                        "файл конфигурации VPN отсутствовал (удалён только профиль)".to_string()
                    }),
                },
                Err(error) => CliReply::Error {
                    message: error.to_string(),
                },
            }
        }
        CliRequest::Doctor => match (manager, wsl) {
            (Some(manager), Some(wsl)) => CliReply::DoctorReport {
                items: crate::doctor::run(wsl, store, manager).await,
            },
            _ => CliReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        CliRequest::Upgrade => {
            let Some(manager) = manager else {
                return CliReply::Error {
                    message: MANAGER_NOT_WIRED.to_string(),
                };
            };
            // Живые бутылки держат сессии аплаенса: сносить дистрибуцию
            // под ними нельзя (Review Focus #2) — проверка до всего.
            let live: Vec<String> = manager
                .status()
                .await
                .into_iter()
                .filter(|state| {
                    matches!(
                        state.state,
                        BottleState::Starting
                            | BottleState::Up
                            | BottleState::Degraded { .. }
                            | BottleState::Stopping
                    )
                })
                .map(|state| state.id)
                .collect();
            if !live.is_empty() {
                return CliReply::Error {
                    message: format!("остановите бутылки перед обновлением: {}", live.join(", ")),
                };
            }
            let (Some(wsl), Some(rootfs)) = (wsl, rootfs) else {
                return CliReply::Error {
                    message: MANAGER_NOT_WIRED.to_string(),
                };
            };
            let previous = manager.appliance_version().await;
            match crate::appliance::upgrade_appliance(
                wsl,
                store,
                rootfs,
                crate::manager::AGENT_CONNECT_TIMEOUT,
            )
            .await
            {
                Ok(current) => {
                    manager.remember_appliance_version(&current).await;
                    CliReply::Upgraded { previous, current }
                }
                Err(error) => CliReply::Error {
                    message: format!("обновление аплаенса не удалось: {error}"),
                },
            }
        }
        CliRequest::RoutesCleanup => {
            if !crate::svc_client::svc_pipe_exists() {
                return CliReply::Done {
                    warning: Some(
                        "служба маршрутов не установлена — снимать маршруты нечего".to_string(),
                    ),
                };
            }
            match crate::svc_client::cleanup_via_service().await {
                Ok(()) => CliReply::Done { warning: None },
                Err(error) => CliReply::Error {
                    message: format!("очистка маршрутов не удалась: {error}"),
                },
            }
        }
        CliRequest::LogsExport { dest } => {
            match crate::export::export_logs(store, manager, dest).await {
                Ok(path) => CliReply::Path(path.to_string_lossy().into_owned()),
                Err(error) => {
                    tracing::warn!(%error, "экспорт журналов не удался");
                    CliReply::Error {
                        message: format!("не удалось выгрузить журналы: {error}"),
                    }
                }
            }
        }
    }
}

/// Собирает строки ответа `List` из [`Store::list`] (сортировка по id —
/// на стороне хранилища). Битые профили пропускаются с warn-логом.
fn list_summaries(store: &Store) -> Vec<BottleSummary> {
    let mut bottles = Vec::new();
    for (id, result) in store.list() {
        match result {
            Ok(profile) => bottles.push(BottleSummary {
                id: profile.id,
                name: profile.name,
                kind: profile.kind,
                // Список не опрашивает менеджер: состояние знает Show/Status.
                state: None,
            }),
            Err(error) => tracing::warn!(%id, %error, "битый профиль пропущен в списке"),
        }
    }
    bottles
}

/// Активное состояние бутылки (держит слушатели/сессию аплаенса);
/// Failed и Stopped — нет.
fn is_active_state(state: &BottleState) -> bool {
    matches!(
        state,
        BottleState::Starting
            | BottleState::Up
            | BottleState::Degraded { .. }
            | BottleState::Stopping
    )
}

/// Создание бутылки: валидация профиля → отказ при занятом id без
/// `force` → сохранение (копия конфига — семантика `Store::save`).
/// Ошибки валидации переводятся в понятные русские тексты create.
async fn create_bottle(store: &Store, profile: &BottleProfile, force: bool) -> CliReply {
    use bottle_core::profile::{validate_profile, ProfileError};
    if let Err(error) = validate_profile(profile) {
        return CliReply::Error {
            message: match error {
                ProfileError::MissingField => concat!(
                    "не заполнены обязательные поля: для env — protocol и config, ",
                    "для host — vpn (клиент) и минимум один endpoint"
                )
                .to_string(),
                ProfileError::InvalidId => format!(
                    "некорректный id «{}»: строчные буквы, цифры и дефисы",
                    profile.id
                ),
                other => format!("профиль не прошёл валидацию: {other}"),
            },
        };
    }
    if !force && store.load(&profile.id).is_ok() {
        return CliReply::Error {
            message: format!(
                "бутылка {} уже существует; используйте --force для перезаписи \
                 (профиль и конфиг будут заменены)",
                profile.id
            ),
        };
    }
    match store.save(profile) {
        Ok(()) => CliReply::Created {
            id: profile.id.clone(),
        },
        Err(error) => CliReply::Error {
            message: error.to_string(),
        },
    }
}

/// Профиль для ответа `Show`: `None` при любой ошибке чтения.
///
/// `NotFound` — ожидаемая ситуация (молча); прочие ошибки (доступ, диск)
/// дополнительно логируются, чтобы не прятать проблемы за «не найден».
fn show_profile(store: &Store, id: &str) -> Option<BottleProfile> {
    match store.load(id) {
        Ok(profile) => Some(profile),
        Err(DaemonError::NotFound { .. }) => None,
        Err(error) => {
            tracing::warn!(%id, %error, "профиль не читается");
            None
        }
    }
}

/// Обслуживает named pipe `pipe_name` параллельно: принятое соединение
/// уходит в свою задачу, цикл немедленно создаёт следующий экземпляр
/// канала — до отмены `shutdown`.
///
/// Это named pipe, а не сокет: параллельные клиенты требуют нескольких
/// экземпляров канала (один клиент занимает экземпляр на всё время
/// соединения), поэтому цикл держит один «ожидающий подключение»
/// экземпляр, а принятые живут в своих задачах. Состояние демона
/// потокобезопасно: [`Store`] — stateless-обёртка над файлами,
/// [`BottleManager`] держит runtime-карту под мьютексами с короткими
/// критическими секциями без `await` (контракт конкурентности в его
/// модуле). Ошибки отдельных соединений логируются и не останавливают
/// сервер; наружу уходит только отказ создать экземпляр канала или сбой
/// ожидания подключения. Отмена `shutdown` завершает акцепт-цикл
/// и все задачи соединений (каждая следит за тем же токеном).
#[cfg(windows)]
pub async fn serve(
    pipe_name: &str,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let mut server = ServerOptions::new().create(pipe_name)?;
    let mut connections: JoinSet<()> = JoinSet::new();
    tracing::info!(pipe = pipe_name, "pipe-сервер запущен");
    loop {
        tokio::select! {
            connected = server.connect() => connected?,
            () = shutdown.cancelled() => {
                tracing::info!("pipe-сервер остановлен по отмене");
                connections.abort_all();
                while connections.join_next().await.is_some() {
                    // Ждём фактического завершения задач (порты/каналы
                    // освобождаются до возврата из serve).
                }
                return Ok(());
            }
        }
        let connection = handle_connection(server, Arc::clone(&daemon), shutdown.clone());
        connections.spawn(connection);
        server = ServerOptions::new().create(pipe_name)?;
    }
}

/// Обслуживает одно подключение CLI: кадры запросов → ответы диспетчера.
///
/// Живёт в своей задаче со своим экземпляром канала (см. [`serve`]):
/// параллельные соединения не ждут друг друга. Завершается при
/// отключении клиента, некорректном кадре или отмене `shutdown`
/// (в том числе посреди исполнения запроса — на остановке демона);
/// после обмена экземпляр канала отключается от клиента.
#[cfg(windows)]
async fn handle_connection(
    pipe: NamedPipeServer,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) {
    let (read_half, mut write_half) = split(pipe);
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<CliRequest>::new());
    let mut reply_codec = LengthJsonCodec::<CliReply>::new();

    loop {
        let req = tokio::select! {
            maybe = reader.next() => match maybe {
                Some(Ok(req)) => req,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "некорректный кадр от CLI, соединение закрывается");
                    break;
                }
                // Клиент отключился.
                None => break,
            },
            () = shutdown.cancelled() => break,
        };

        // Исполнение тоже отменяемо: остановка демона не ждёт долгих
        // команд (start с handshake туннеля); незавершённые операции
        // менеджера разбираются штатными путями его контракта.
        let reply = tokio::select! {
            reply = dispatch(
                &daemon.store,
                daemon.manager.as_deref(),
                daemon.wsl.as_deref(),
                daemon.rootfs.as_deref(),
                req,
            ) => reply,
            () = shutdown.cancelled() => break,
        };
        if let Err(e) = send_reply(&mut write_half, &mut reply_codec, reply).await {
            tracing::warn!(error = %e, "не удалось отправить ответ CLI");
            break;
        }
    }

    let read_half = reader.into_inner();
    // unsplit возвращает исходный канал напрямую (половины из одного split).
    let pipe = read_half.unsplit(write_half);
    if let Err(e) = pipe.disconnect() {
        tracing::warn!(error = %e, "disconnect экземпляра канала не удался");
    }
}

/// Кодирует и отправляет ответ CLI.
///
/// Используется fallible-путь `Encoder` (а не паникующий `encode_frame`).
#[cfg(windows)]
async fn send_reply<W>(
    write_half: &mut W,
    codec: &mut LengthJsonCodec<CliReply>,
    reply: CliReply,
) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    codec.encode(reply, &mut buf).map_err(io::Error::other)?;
    write_half.write_all(&buf).await
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use bottle_core::profile::{
        validate_profile, BottleKind, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig,
    };

    /// Env-профиль с исходным конфигом `config` (до нормализации save).
    fn env_profile(id: &str, config: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Бутылка {id}"),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: config.to_string(),
            },
            expose: ExposeConfig {
                socks5: 0,
                forwards: vec![ForwardSpec {
                    listen: 0,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some("10.20.30.1".parse().unwrap()),
                launch: Some("mstsc".to_string()),
            },
        }
    }

    /// Host-профиль (не требует файла конфига в хранилище).
    fn host_profile(id: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Хост {id}"),
            kind: BottleKind::Host,
            vpn: VpnConfig::Host {
                client: r"C:\Program Files\Vendor\vpn.exe".to_string(),
                args: Some("connect office".to_string()),
                endpoints: vec!["vpn.example.com".to_string()],
                interface: Some("Vendor Adapter".to_string()),
            },
            expose: ExposeConfig {
                socks5: 1080,
                forwards: Vec::new(),
                dns: None,
                launch: None,
            },
        }
    }

    /// Хранилище с двумя валидными профилями и одним битым
    /// (id внутри файла не совпадает с именем файла).
    fn store_with_broken_profile() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store
            .save(&env_profile("office-a", src.to_str().unwrap()))
            .unwrap();
        store.save(&host_profile("corp-vpn")).unwrap();
        fs::write(
            root.join("bottles").join("broken.toml"),
            r#"
id = "other"
name = "Чужой"
kind = "host"

[vpn]
client = "vpn.exe"
endpoints = ["e.example"]
"#,
        )
        .unwrap();
        (tmp, store)
    }

    /// Пустое хранилище (для команд, не читающих профили).
    fn empty_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        (tmp, store)
    }

    fn summary(id: &str, name: &str, kind: BottleKind) -> BottleSummary {
        BottleSummary {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            state: None,
        }
    }

    #[tokio::test]
    async fn list_returns_summaries_of_valid_profiles_only() {
        let (_tmp, store) = store_with_broken_profile();

        let reply = dispatch(&store, None, None, None, CliRequest::List).await;

        let CliReply::List { bottles } = reply else {
            panic!("ожидали List, получили {reply:?}");
        };
        assert_eq!(
            bottles,
            vec![
                summary("corp-vpn", "Хост corp-vpn", BottleKind::Host),
                summary("office-a", "Бутылка office-a", BottleKind::Env),
            ]
        );
    }

    #[tokio::test]
    async fn list_over_empty_store_is_empty() {
        let (_tmp, store) = empty_store();

        let reply = dispatch(&store, None, None, None, CliRequest::List).await;

        assert_eq!(
            reply,
            CliReply::List { bottles: vec![] },
            "пустое хранилище — пустой список, не ошибка"
        );
    }

    #[tokio::test]
    async fn show_returns_profile_without_runtime() {
        let (_tmp, store) = store_with_broken_profile();

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Show {
                id: "corp-vpn".to_string(),
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Show {
                profile: Some(host_profile("corp-vpn")),
                runtime: None,
            }
        );
    }

    #[tokio::test]
    async fn show_missing_profile_answers_empty_show() {
        let (_tmp, store) = store_with_broken_profile();

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Show {
                id: "ghost".to_string(),
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Show {
                profile: None,
                runtime: None,
            }
        );
    }

    #[tokio::test]
    async fn start_stop_status_answer_transitional_error() {
        let (_tmp, store) = empty_store();
        let commands = vec![
            CliRequest::Start {
                id: "office-a".to_string(),
            },
            CliRequest::Stop {
                id: "office-a".to_string(),
            },
            CliRequest::Status,
            CliRequest::Doctor,
        ];

        for req in commands {
            assert_eq!(
                dispatch(&store, None, None, None, req).await,
                CliReply::Error {
                    message: MANAGER_NOT_WIRED.to_string(),
                },
                "без менеджера команды управления отвечают переходной ошибкой"
            );
        }
    }

    /// Создание env-бутылки: конфиг скопирован, профиль записан.
    #[tokio::test]
    async fn create_env_copies_config_and_writes_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Create {
                profile: env_profile("office-a", src.to_str().unwrap()),
                force: false,
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Created {
                id: "office-a".into()
            }
        );
        assert_eq!(
            fs::read_to_string(root.join("bottles").join("office-a.conf")).unwrap(),
            "wg-secret"
        );
        assert!(root.join("bottles").join("office-a.toml").exists());
    }

    /// Занятый id — отказ без --force (Review Focus M3 #5): профиль и
    /// конфиг пользователя не перезатираются.
    #[tokio::test]
    async fn create_taken_id_is_refused_without_force() {
        let (_tmp, store) = store_with_broken_profile();
        let src = store.data_root().join("bottles").join("office-a.conf");

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Create {
                profile: env_profile("office-a", src.to_str().unwrap()),
                force: false,
            },
        )
        .await;

        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(message.contains("--force"), "подсказка --force: {message}");
    }

    /// Создание с --force перезаписывает существующий профиль.
    #[tokio::test]
    async fn create_with_force_overwrites() {
        let (_tmp, store) = store_with_broken_profile();
        let src = store.data_root().join("bottles").join("office-a.conf");

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Create {
                profile: env_profile("office-a", src.to_str().unwrap()),
                force: true,
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Created {
                id: "office-a".into()
            }
        );
    }

    /// host без endpoint'ов — ошибка валидации при создании.
    #[tokio::test]
    async fn create_host_without_endpoints_is_rejected() {
        let (_tmp, store) = empty_store();
        let mut profile = host_profile("corp-vpn");
        if let VpnConfig::Host { endpoints, .. } = &mut profile.vpn {
            endpoints.clear();
        }
        assert!(validate_profile(&profile).is_err(), "профиль невалиден");

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Create {
                profile,
                force: false,
            },
        )
        .await;

        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("endpoint") || message.contains("поле"),
            "понятная ошибка: {message}"
        );
    }

    /// Удаление остановленной бутылки: файлы снесены.
    #[tokio::test]
    async fn delete_stopped_removes_files() {
        let (_tmp, store) = store_with_broken_profile();

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Delete {
                id: "office-a".to_string(),
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Deleted {
                id: "office-a".into(),
                warning: None
            }
        );
        assert!(!store
            .data_root()
            .join("bottles")
            .join("office-a.toml")
            .exists());
        assert!(!store
            .data_root()
            .join("bottles")
            .join("office-a.conf")
            .exists());
    }

    /// Удаление отсутствующего профиля — отказ, не Created-подобный
    /// молчаливый успех.
    #[tokio::test]
    async fn delete_missing_profile_is_error() {
        let (_tmp, store) = empty_store();

        let reply = dispatch(
            &store,
            None,
            None,
            None,
            CliRequest::Delete {
                id: "ghost".to_string(),
            },
        )
        .await;

        assert!(matches!(reply, CliReply::Error { .. }));
    }

    /// Routes cleanup без установленной службы — Done с предупреждением,
    /// не ошибка (portable-режим; на не-windows канала службы нет).
    #[tokio::test]
    async fn routes_cleanup_without_service_warns() {
        let (_tmp, store) = empty_store();

        let reply = dispatch(&store, None, None, None, CliRequest::RoutesCleanup).await;

        match reply {
            CliReply::Done {
                warning: Some(warning),
            } => {
                assert!(warning.contains("служба"), "причина: {warning}");
            }
            other => panic!("ожидали Done с предупреждением, получили {other:?}"),
        }
    }

    /// Имя тестового канала: уникально в пределах прогона (параллельные
    /// тесты не конфликтуют ни между собой, ни с боевым `bottled`).
    #[cfg(windows)]
    fn unique_pipe_name(label: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(r"\\.\pipe\bottled-test-{label}-{}-{n}", std::process::id())
    }

    /// Подключается к тестовому каналу с повторами: между созданием
    /// очередного ожидающего экземпляра и принятием предыдущего клиента
    /// видит «канал занят» или «канал ещё не создан» — это ожидаемо,
    /// а не ошибка.
    #[cfg(windows)]
    async fn connect_client(pipe_name: &str) -> tokio::net::windows::named_pipe::NamedPipeClient {
        use tokio::net::windows::named_pipe::ClientOptions;
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

        for _ in 0..500 {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => return client,
                Err(e)
                    if matches!(e.raw_os_error(),
                    Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(e) => panic!("не удалось подключиться к каналу {pipe_name}: {e}"),
            }
        }
        panic!("канал {pipe_name} не стал доступен за 5 секунд");
    }

    /// Обмен «запрос → ответ» через клиентский конец канала.
    #[cfg(windows)]
    async fn exchange(
        client: &mut tokio::net::windows::named_pipe::NamedPipeClient,
        req: CliRequest,
    ) -> CliReply {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_util::codec::Decoder;

        client
            .write_all(&bottle_core::frame::encode_frame(&req))
            .await
            .expect("запрос отправлен");

        let mut codec = super::LengthJsonCodec::<CliReply>::new();
        let mut buf = bytes::BytesMut::new();
        loop {
            if let Some(reply) = codec.decode(&mut buf).expect("ответ разбирается")
            {
                return reply;
            }
            let mut chunk = [0u8; 4096];
            let n = client.read(&mut chunk).await.expect("чтение ответа");
            assert!(n > 0, "сервер закрыл канал до ответа");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_serves_list_show_and_missing_profile() {
        use tokio_util::sync::CancellationToken;

        let (_tmp, store) = store_with_broken_profile();
        let pipe_name = unique_pipe_name("basic");
        let shutdown = CancellationToken::new();
        let daemon = Arc::new(crate::Daemon {
            store,
            manager: None,
            wsl: None,
            rootfs: None,
        });
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        let mut client = connect_client(&pipe_name).await;

        // List: два валидных профиля, битый пропущен, состояние неизвестно.
        let reply = exchange(&mut client, CliRequest::List).await;
        let CliReply::List { bottles } = reply else {
            panic!("ожидали List, получили {reply:?}");
        };
        assert_eq!(
            bottles.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["corp-vpn", "office-a"]
        );
        assert!(
            bottles.iter().all(|b| b.state.is_none()),
            "список не опрашивает менеджер"
        );

        // Show существующего: профиль с диска, рантайма нет.
        let reply = exchange(
            &mut client,
            CliRequest::Show {
                id: "corp-vpn".to_string(),
            },
        )
        .await;
        assert_eq!(
            reply,
            CliReply::Show {
                profile: Some(host_profile("corp-vpn")),
                runtime: None,
            }
        );

        // Show отсутствующего: пустой ответ, не ошибка.
        let reply = exchange(
            &mut client,
            CliRequest::Show {
                id: "ghost".to_string(),
            },
        )
        .await;
        assert_eq!(
            reply,
            CliReply::Show {
                profile: None,
                runtime: None,
            }
        );

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_serves_sequential_clients() {
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store.save(&host_profile("solo")).unwrap();

        let pipe_name = unique_pipe_name("seq");
        let shutdown = CancellationToken::new();
        let daemon = Arc::new(crate::Daemon {
            store,
            manager: None,
            wsl: None,
            rootfs: None,
        });
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        for round in 0..2 {
            let mut client = connect_client(&pipe_name).await;
            let reply = exchange(&mut client, CliRequest::List).await;
            let CliReply::List { bottles } = reply else {
                panic!("ожидали List, получили {reply:?}");
            };
            assert_eq!(bottles.len(), 1, "раунд {round}: один профиль");
            assert_eq!(bottles[0].id, "solo");
            // Клиент отключается в конце итерации; его экземпляр канала
            // уходит с задачей, следующий клиент берёт свежий ожидающий
            // экземпляр (connect_client пережидает стык).
        }

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }

    /// T13: параллельное обслуживание — два ОДНОВРЕМЕННЫХ клиента.
    ///
    /// Первый держит длинный `start` (фейк-менеджер с задержкой подключения
    /// к аплаенсу 2 с), второй за это время получает ответ `List`: таймаут
    /// ассерта меньше задержки, поэтому последовательное обслуживание
    /// (ответ второму только после первого) тест проваливает.
    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_answers_second_client_while_start_is_in_flight() {
        use std::time::Duration;

        use tokio_util::sync::CancellationToken;

        use crate::appliance::ApplianceHandle;
        use crate::manager::{AgentConnect, BottleManager};

        const AGENT_DELAY: Duration = Duration::from_secs(2);
        const SECOND_CLIENT_TIMEOUT: Duration = Duration::from_secs(1);

        /// Фейк-подключение к аплаенсу с задержкой: `start` уходит в
        /// `connect` и висит `AGENT_DELAY`, после чего отказывается.
        /// Достаточно для параллельного стенда — бутылка не поднимается.
        struct SlowAgentConnect;

        #[async_trait::async_trait]
        impl AgentConnect for SlowAgentConnect {
            async fn connect(&self) -> Result<ApplianceHandle, DaemonError> {
                tokio::time::sleep(AGENT_DELAY).await;
                Err(DaemonError::Appliance(
                    "фейк-аплаенс с задержкой: соединение не состоялось".to_string(),
                ))
            }

            async fn alive(&self) -> bool {
                false
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        let src = tmp.path().join("wg-office.conf");
        fs::write(&src, "wg-secret").unwrap();
        store
            .save(&env_profile("office-a", src.to_str().unwrap()))
            .unwrap();

        // Менеджер владеет своим экземпляром хранилища того же корня
        // (паттерн main::serve_daemon).
        let manager_store = Store::open(store.data_root()).unwrap();
        let manager = Arc::new(BottleManager::new(
            manager_store,
            Arc::new(SlowAgentConnect),
        ));

        let pipe_name = unique_pipe_name("par");
        let shutdown = CancellationToken::new();
        let daemon = Arc::new(crate::Daemon {
            store,
            manager: Some(manager),
            wsl: None,
            rootfs: None,
        });
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        // Первый клиент отправляет start и держит соединение открытым,
        // пока ответ не придёт (задача, не await).
        let mut slow_client = connect_client(&pipe_name).await;
        let slow_reply = tokio::spawn(async move {
            exchange(
                &mut slow_client,
                CliRequest::Start {
                    id: "office-a".to_string(),
                },
            )
            .await
        });
        // Даём серверу войти в диспетчер (до задержки фейк-агента).
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Второй клиент: List отвечает, НЕ дожидаясь чужого start.
        let mut fast_client = connect_client(&pipe_name).await;
        let reply = tokio::time::timeout(
            SECOND_CLIENT_TIMEOUT,
            exchange(&mut fast_client, CliRequest::List),
        )
        .await
        .expect("List не должен ждать чужой start (параллельные клиенты)");
        let CliReply::List { bottles } = reply else {
            panic!("ожидали List, получили {reply:?}");
        };
        assert_eq!(
            bottles.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["office-a"]
        );
        drop(fast_client);

        // Первый клиент в итоге получает ошибку (после задержки фейка).
        let slow_reply = tokio::time::timeout(AGENT_DELAY * 5, slow_reply)
            .await
            .expect("start отвечает после задержки фейк-агента (сервер не потерял соединение)")
            .expect("задача slow-клиента не паниковала");
        assert!(
            matches!(slow_reply, CliReply::Error { .. }),
            "start против фейка с отказом → Error, получено {slow_reply:?}"
        );

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }
}
