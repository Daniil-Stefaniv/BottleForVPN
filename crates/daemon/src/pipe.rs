//! Диспетчер служебных команд CLI и named-pipe транспорт демона.
//!
//! Протокол 0.3 (GUI↔демон) разбирает [`crate::session`]; сюда попадают
//! кадры [`CliRequest`], которые шлёт CLI `pogrebok`. В 0.3 команды
//! управления бутылками удалены с провода целиком: CLI отвечает на них
//! надгробием локально (не касаясь демона), а здесь остались только
//! служебные `Doctor`/`LogsExport`/`Upgrade`/`RoutesCleanup`/
//! `ShutdownDaemon`.
//!
//! Транспорт (Windows named pipe `\\.\pipe\pogrebok-daemon`, [`serve`])
//! обслуживает клиентов параллельно: принятое соединение уходит в свою
//! задачу `tokio::spawn`, а цикл сразу пересоздаёт экземпляр канала для
//! следующих подключений. Одновременно обслуживается столько клиентов,
//! сколько живых экземпляров канала (T13).

use bottle_core::ipc::{BottleState, CliReply, CliRequest};

use crate::Daemon;

#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
#[cfg(windows)]
use tokio::task::JoinSet;
#[cfg(windows)]
use tokio_util::sync::CancellationToken;

/// Переходный ответ, когда менеджер бутылок не подключён (только тесты
/// и dev-сборки без аплаенса).
const MANAGER_NOT_WIRED: &str = "менеджер бутылок не подключён";

/// Диспетчер служебных команд CLI (0.3).
pub async fn dispatch(
    daemon: &Daemon,
    shutdown: Option<&tokio_util::sync::CancellationToken>,
    req: CliRequest,
) -> CliReply {
    match req {
        CliRequest::Doctor => match (&daemon.manager, &daemon.wsl) {
            (Some(manager), Some(wsl)) => CliReply::DoctorReport {
                items: crate::doctor::run(wsl.as_ref(), &daemon.registry, manager).await,
            },
            _ => CliReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        CliRequest::Upgrade { force } => {
            let Some(manager) = &daemon.manager else {
                return CliReply::Error {
                    message: MANAGER_NOT_WIRED.to_string(),
                };
            };
            if force {
                // Полный сброс: бутылки останавливаются принудительно,
                // дальше appliance гасит WSL целиком и пере-импортирует
                // дистрибуцию (кнопка «Сбросить аплаенс» в GUI).
                let failed = manager.stop_all().await;
                if !failed.is_empty() {
                    return CliReply::Error {
                        message: format!(
                            "не удалось остановить бутылки: {} — остановите их вручную и повторите",
                            failed.join(", ")
                        ),
                    };
                }
            } else {
                // Живые бутылки держат сессии аплаенса: сносить
                // дистрибуцию под ними нельзя (Review Focus #2) —
                // проверка до всего.
                let live: Vec<String> = manager
                    .status()
                    .await
                    .into_iter()
                    .filter(|state| is_active_state(&state.state))
                    .map(|state| state.id)
                    .collect();
                if !live.is_empty() {
                    return CliReply::Error {
                        message: format!(
                            "остановите бутылки перед обновлением: {} \
                             (или `pogrebok upgrade --force` — они будут остановлены)",
                            live.join(", ")
                        ),
                    };
                }
            }
            let (Some(wsl), Some(rootfs)) = (&daemon.wsl, &daemon.rootfs) else {
                return CliReply::Error {
                    message: MANAGER_NOT_WIRED.to_string(),
                };
            };
            // Токен аплаенса живёт в каталоге данных демона (paths).
            let previous = manager.appliance_version().await;
            match crate::appliance::upgrade_appliance(
                wsl.as_ref(),
                &daemon.data_root,
                rootfs,
                crate::manager::AGENT_CONNECT_TIMEOUT,
                force,
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
        CliRequest::ShutdownDaemon => {
            if let Some(manager) = &daemon.manager {
                let failed = manager.stop_all().await;
                if !failed.is_empty() {
                    tracing::warn!(ids = ?failed, "часть бутылок не остановилась при shutdown");
                }
            }
            // Отменяем serve с задержкой: кадр ответа должен уйти
            // клиенту до остановки транспорта. Без токена (тесты) —
            // просто отвечаем.
            if let Some(shutdown) = shutdown {
                let token = shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    token.cancel();
                });
            }
            CliReply::Done {
                warning: Some("демон останавливается".to_string()),
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
            let profiles = {
                let registry = daemon.registry.read().expect("реестр профилей");
                registry
                    .ids()
                    .into_iter()
                    .filter_map(|id| registry.get(&id).map(|full| full.profile))
                    .collect::<Vec<_>>()
            };
            match crate::export::export_logs(
                &daemon.data_root,
                &profiles,
                daemon.manager.as_deref(),
                dest,
            )
            .await
            {
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

/// Активное состояние бутылки (держит слушатели/сессию аплаенса);
/// Failed и Stopped — нет.
pub(crate) fn is_active_state(state: &BottleState) -> bool {
    matches!(
        state,
        BottleState::Starting
            | BottleState::Up
            | BottleState::Degraded { .. }
            | BottleState::Stopping
    )
}

/// Обслуживает named pipe `pipe_name` параллельно: принятое соединение
/// уходит в свою задачу, цикл немедленно создаёт следующий экземпляр
/// канала — до отмены `shutdown`.
///
/// Это named pipe, а не сокет: параллельные клиенты требуют нескольких
/// экземпляров канала (один клиент занимает экземпляр на всё время
/// соединения), поэтому цикл держит один «ожидающий подключение»
/// экземпляр, а принятые живут в своих задачах. Ошибки отдельных
/// соединений логируются и не останавливают сервер; наружу уходит
/// только отказ создать экземпляр канала или сбой ожидания подключения.
/// Отмена `shutdown` завершает акцепт-цикл и все задачи соединений
/// (каждая следит за тем же токеном).
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

/// Обслуживает одно подключение: протокол сессии единый с UDS
/// ([`crate::session`]). Живёт в своей задаче со своим экземпляром
/// канала (см. [`serve`]): параллельные соединения не ждут друг друга.
#[cfg(windows)]
async fn handle_connection(
    pipe: NamedPipeServer,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) {
    crate::session::handle_connection(pipe, daemon, shutdown).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use bottle_core::ipc::GuiRequest;

    /// Doctor без подключённого менеджера — понятная ошибка (dev-сборки).
    #[tokio::test]
    async fn doctor_without_manager_is_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);

        let reply = dispatch(&daemon, None, CliRequest::Doctor).await;

        let CliReply::Error { message } = reply else {
            panic!("ожидали Error без менеджера, получили {reply:?}");
        };
        assert!(message.contains("менеджер"), "{message}");
    }

    /// Сервисные команды живут: routes cleanup без службы — Done с
    /// предупреждением, не ошибка.
    #[tokio::test]
    async fn routes_cleanup_without_service_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);

        let reply = dispatch(&daemon, None, CliRequest::RoutesCleanup).await;

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
    /// тесты не конфликтуют ни между собой, ни с боевым `pogrebok-daemon`).
    #[cfg(windows)]
    fn unique_pipe_name(label: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(r"\\.\pipe\pogrebok-daemon-test-{label}-{}-{n}", std::process::id())
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

    /// Обмен «запрос → ответ» кадрами Value (нейтральный формат сессии
    /// 0.3: один канал несёт и GuiRequest/GuiReply, и legacy-кадры).
    #[cfg(windows)]
    async fn exchange(
        client: &mut tokio::net::windows::named_pipe::NamedPipeClient,
        req: &impl serde::Serialize,
    ) -> serde_json::Value {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_util::codec::{Decoder, Encoder};

        let mut codec = bottle_core::frame::LengthJsonCodec::<serde_json::Value>::new();
        let mut buf = bytes::BytesMut::new();
        codec
            .encode(serde_json::to_value(req).expect("запрос в Value"), &mut buf)
            .expect("запрос кодируется");
        client.write_all(&buf).await.expect("запрос отправлен");

        let mut read_codec = bottle_core::frame::LengthJsonCodec::<serde_json::Value>::new();
        let mut buf = bytes::BytesMut::new();
        loop {
            if let Some(reply) = read_codec.decode(&mut buf).expect("ответ разбирается") {
                return reply;
            }
            let mut chunk = [0u8; 4096];
            let n = client.read(&mut chunk).await.expect("чтение ответа");
            assert!(n > 0, "сервер закрыл канал до ответа");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Env-профиль для windows-тестов транспорта.
    #[cfg(windows)]
    fn env_profile(id: &str, config: &str) -> bottle_core::profile::BottleProfile {
        use bottle_core::profile::*;
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

    /// Протокол 0.3 поверх named pipe: auth-гейт (Hello), затем
    /// SyncProfiles; legacy List — надгробие.
    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_serves_gui_protocol_and_legacy_tombstone() {
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let pipe_name = unique_pipe_name("gui");
        let shutdown = CancellationToken::new();
        let daemon = crate::test_daemon(tmp.path(), "tok1", None);
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        let mut client = connect_client(&pipe_name).await;

        // До Hello управление закрыто.
        let reply = exchange(&mut client, &GuiRequest::Stats).await;
        let reply: bottle_core::ipc::GuiReply =
            serde_json::from_value(reply).expect("GuiReply");
        assert!(
            matches!(&reply, bottle_core::ipc::GuiReply::Error { message } if message.starts_with("AUTH")),
            "до Hello — AUTH: {reply:?}"
        );

        // Hello с верным токеном → Ok; SyncProfiles принимается.
        let reply = exchange(
            &mut client,
            &GuiRequest::Hello {
                token: "tok1".to_string(),
            },
        )
        .await;
        let reply: bottle_core::ipc::GuiReply =
            serde_json::from_value(reply).expect("GuiReply");
        assert_eq!(reply, bottle_core::ipc::GuiReply::Ok);

        let reply = exchange(&mut client, &GuiRequest::SyncProfiles { profiles: vec![] }).await;
        let reply: bottle_core::ipc::GuiReply =
            serde_json::from_value(reply).expect("GuiReply");
        assert_eq!(reply, bottle_core::ipc::GuiReply::Ok);

        // Служебный Doctor по legacy-кадру: без менеджера — понятная
        // ошибка, но кадр распознаётся и исполняется.
        let reply = exchange(&mut client, &CliRequest::Doctor).await;
        let reply: CliReply = serde_json::from_value(reply).expect("CliReply");
        assert!(
            matches!(&reply, CliReply::Error { message } if message.contains("менеджер")),
            "Doctor без менеджера: {reply:?}"
        );

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }

    /// Последовательные клиенты: каждый получает свой экземпляр канала.
    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_serves_sequential_clients() {
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let pipe_name = unique_pipe_name("seq");
        let shutdown = CancellationToken::new();
        let daemon = crate::test_daemon(tmp.path(), "tok1", None);
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        for round in 0..2 {
            let mut client = connect_client(&pipe_name).await;
            let reply = exchange(&mut client, &CliRequest::Doctor).await;
            let reply: CliReply = serde_json::from_value(reply).expect("CliReply");
            assert!(
                matches!(&reply, CliReply::Error { message } if message.contains("менеджер")),
                "раунд {round}: Doctor без менеджера"
            );
            // Клиент отключается в конце итерации; его экземпляр канала
            // уходит с задачей, следующий клиент берёт свежий ожидающий
            // экземпляр (connect_client пережидает стык).
        }

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }

    /// T13: параллельное обслуживание — два ОДНОВРЕМЕННЫХ клиента.
    ///
    /// Первый держит длинный `StartBottle` (фейк-менеджер с задержкой
    /// подключения к аплаенсу 2 с), второй за это время получает ответ
    /// на служебный Doctor (мгновенно): таймаут ассерта меньше задержки,
    /// поэтому последовательное обслуживание тест проваливает.
    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_answers_second_client_while_start_is_in_flight() {
        use std::time::Duration;

        use tokio_util::sync::CancellationToken;

        use crate::appliance::ApplianceHandle;
        use crate::error::DaemonError;
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
        let registry = crate::registry::Registry::new_shared();
        registry
            .write()
            .expect("реестр")
            .upsert(bottle_core::ipc::FullProfile {
                profile: env_profile("office-a", "office-a.conf"),
                config_text: Some("wg-secret".to_string()),
                credentials: None,
            });
        let manager = Arc::new(BottleManager::new(
            registry.clone(),
            tmp.path().join("manager-data"),
            Arc::new(SlowAgentConnect),
        ));
        let daemon = crate::test_daemon(tmp.path(), "tok1", Some(manager));
        // Реестр демона — тот же, что у менеджера (в бою их наполняет
        // один SyncProfiles).
        daemon
            .registry
            .write()
            .expect("реестр")
            .upsert(bottle_core::ipc::FullProfile {
                profile: env_profile("office-a", "office-a.conf"),
                config_text: Some("wg-secret".to_string()),
                credentials: None,
            });

        let pipe_name = unique_pipe_name("par");
        let shutdown = CancellationToken::new();
        let (server_pipe_name, server_shutdown) = (pipe_name.clone(), shutdown.clone());
        let server =
            tokio::spawn(async move { serve(&server_pipe_name, daemon, server_shutdown).await });

        // Первый клиент: Hello + StartBottle (висит в connect AGENT_DELAY).
        let mut first = connect_client(&pipe_name).await;
        let reply = exchange(
            &mut first,
            &GuiRequest::Hello {
                token: "tok1".to_string(),
            },
        )
        .await;
        let reply: bottle_core::ipc::GuiReply =
            serde_json::from_value(reply).expect("GuiReply");
        assert_eq!(reply, bottle_core::ipc::GuiReply::Ok);
        let start = tokio::spawn(async move {
            exchange(
                &mut first,
                &GuiRequest::StartBottle {
                    id: "office-a".to_string(),
                },
            )
            .await;
        });

        // Второй клиент получает ответ за время «зависшего» первого.
        let second = tokio::spawn({
            let pipe_name = pipe_name.clone();
            async move {
                let mut client = connect_client(&pipe_name).await;
                exchange(&mut client, &CliRequest::Doctor).await
            }
        });
        let reply = tokio::time::timeout(SECOND_CLIENT_TIMEOUT, second)
            .await
            .expect("второй клиент обслужен, пока первый висит в start")
            .expect("задача второго клиента");
        let reply: CliReply = serde_json::from_value(reply).expect("CliReply");
        assert!(
            matches!(reply, CliReply::Error { .. } | CliReply::DoctorReport { .. }),
            "Doctor обслужен параллельно: {reply:?}"
        );

        start.await.expect("первый клиент дождался ответа");
        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }
}
