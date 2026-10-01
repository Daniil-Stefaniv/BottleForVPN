//! Канал управления демона: диспетчер команд CLI и named-pipe транспорт.
//!
//! [`dispatch`] — чистая функция над хранилищем, исполняется на любой
//! платформе и покрывается unit-тестами. Транспорт (Windows named pipe
//! `\\.\pipe\bottled`, [`serve`]) обслуживает клиентов по одному: для
//! каждого соединения создаётся свежий экземпляр канала, после отключения
//! клиента — следующий.

use bottle_core::ipc::{BottleSummary, CliReply, CliRequest};
use bottle_core::profile::BottleProfile;

use crate::error::DaemonError;
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
use tokio_stream::StreamExt;
#[cfg(windows)]
use tokio_util::codec::{Encoder, FramedRead};
#[cfg(windows)]
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use crate::Daemon;

/// Переходный ответ на команды управления до Task 16 (менеджер бутылок).
const COMMAND_NOT_WIRED: &str = "команда ещё не подключена (Task 16)";

/// Диспетчер команд CLI: обслуживает запрос из хранилища `store`.
///
/// Чистая функция (побочные эффекты — только чтение хранилища и warn-логи);
/// асинхронность — контракт для Task 16, где Start/Stop/Status уйдут
/// в менеджер бутылок. `List` возвращает только валидные профили: для
/// битого файла честных полей строки списка нет (тип неизвестен), поэтому
/// он пропускается с warn-логом. `Show` на отсутствующем/битом профиле
/// отвечает пустым ответом — `profile: None` (клиент показывает «не найден»).
// Вызывается windows-транспортом; на остальных платформах — только тестами.
#[cfg_attr(not(windows), allow(dead_code))]
pub async fn dispatch(store: &Store, req: CliRequest) -> CliReply {
    match req {
        CliRequest::List => CliReply::List {
            bottles: list_summaries(store),
        },
        CliRequest::Show { id } => CliReply::Show {
            profile: show_profile(store, &id),
            // Рантайм-состояние появится вместе с менеджером (Task 16).
            runtime: None,
        },
        CliRequest::Start { .. } | CliRequest::Stop { .. } | CliRequest::Status => {
            CliReply::Error {
                message: COMMAND_NOT_WIRED.to_string(),
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
                // До Task 16 демон не поднимает бутылки — состояния нет.
                state: None,
            }),
            Err(error) => tracing::warn!(%id, %error, "битый профиль пропущен в списке"),
        }
    }
    bottles
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

/// Обслуживает named pipe `pipe_name`: по одному клиенту на соединение,
/// свежий экземпляр канала на каждый цикл, до отмены `shutdown`.
///
/// Это named pipe, а не сокет: «следующий клиент» появляется только после
/// создания очередного экземпляра канала — цикл пересоздаёт его после
/// каждого соединения. Ошибки отдельных соединений логируются и не
/// останавливают сервер; наружу уходит только отказ создать экземпляр
/// канала или сбой ожидания подключения.
#[cfg(windows)]
pub async fn serve(
    pipe_name: &str,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let mut server = ServerOptions::new().create(pipe_name)?;
    tracing::info!(pipe = pipe_name, "pipe-сервер запущен");
    loop {
        tokio::select! {
            connected = server.connect() => connected?,
            () = shutdown.cancelled() => {
                tracing::info!("pipe-сервер остановлен по отмене");
                return Ok(());
            }
        }
        handle_connection(server, &daemon, &shutdown).await;
        server = ServerOptions::new().create(pipe_name)?;
    }
}

/// Обслуживает одно подключение CLI: кадры запросов → ответы диспетчера.
///
/// Завершается при отключении клиента, некорректном кадре или отмене
/// `shutdown`; после обмена экземпляр канала отключается от клиента
/// (следующий обслуживается свежим экземпляром, см. [`serve`]).
#[cfg(windows)]
async fn handle_connection(pipe: NamedPipeServer, daemon: &Daemon, shutdown: &CancellationToken) {
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

        let reply = dispatch(&daemon.store, req).await;
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
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig};

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

        let reply = dispatch(&store, CliRequest::List).await;

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

        let reply = dispatch(&store, CliRequest::List).await;

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
        ];

        for req in commands {
            assert_eq!(
                dispatch(&store, req).await,
                CliReply::Error {
                    message: COMMAND_NOT_WIRED.to_string(),
                },
                "до Task 16 команды управления отвечают переходной ошибкой"
            );
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

    /// Подключается к тестовому каналу с повторами: сервер обслуживает
    /// клиентов по одному, и на стыке соединений клиент видит «канал занят»
    /// или «канал ещё не создан» — это ожидаемо, а не ошибка.
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
        let daemon = Arc::new(crate::Daemon { store });
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
            "до Task 16 рантайм-состояния нет"
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
        let daemon = Arc::new(crate::Daemon { store });
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
            // Клиент отключается в конце итерации — следующий обслуживается
            // свежим экземпляром канала (connect_client пережидает стык).
        }

        shutdown.cancel();
        server.await.unwrap().expect("serve завершился без ошибки");
    }
}
