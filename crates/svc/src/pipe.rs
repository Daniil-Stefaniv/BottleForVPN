//! Pipe-сервер службы маршрутов: named pipe [`SVC_PIPE`], кадры
//! `LengthJsonCodec<SvcRequest/SvcReply>`, диспетчер [`dispatch`]
//! (задача 6).
//!
//! Клиенты обслуживаются по одному (демон один; фоновый поток на
//! соединение не нужен): после отключения создаётся свежий экземпляр
//! канала. Каждое соединение — сессия демона: `Hello{daemon_pid}`
//! выделяет монотонный [`SessionId`] и привязывает его к соединению;
//! повторный `Hello` на живом соединении закрывает прежнюю сессию
//! (демон перезапустился) и открывает новую. Когда соединение
//! закрывается — демон умер или отключился — маршруты сессии
//! снимаются из live-set с записью в журнал и удаляются из ОС
//! (спека §9: маршруты не переживают смерть демона).

use bottle_core::svcrpc::{svc_err, validate_route, SvcReply, SvcRequest};
use tokio::sync::Mutex;

use crate::journal::{RouteStore, SessionId};
use crate::routes::{route_ps_args, run_ps, RouteCmd};

/// Предельная длина псевдонима интерфейса (защита в глубину).
pub const MAX_IFACE_ALIAS_LEN: usize = 64;

/// Лёгкая валидация псевдонима интерфейса: непустой, не длиннее
/// [`MAX_IFACE_ALIAS_LEN`], без управляющих символов. Само имя
/// проверяет ОС при выполнении командлета — это защита в глубину
/// против мусора в канале.
pub fn validate_iface_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        Err("псевдоним интерфейса не задан".to_string())
    } else if alias.chars().count() > MAX_IFACE_ALIAS_LEN {
        Err(format!(
            "псевдоним интерфейса длиннее {MAX_IFACE_ALIAS_LEN} символов"
        ))
    } else if alias.chars().any(char::is_control) {
        Err("псевдоним интерфейса содержит управляющие символы".to_string())
    } else {
        Ok(())
    }
}

/// Диспетчер запросов демона: чистая по побочным эффектам кроме
/// операций над маршрутами ОС (только через [`route_ps_args`]) и
/// журнала. `session` — текущая сессия соединения: `Hello` выделяет
/// новую, прочие запросы требуют открытую сессию.
pub async fn dispatch(
    store: &Mutex<RouteStore>,
    session: &mut Option<SessionId>,
    req: SvcRequest,
) -> SvcReply {
    match req {
        SvcRequest::Hello { daemon_pid } => {
            // Повторный Hello на живом соединении — демон перезапустился:
            // прежняя сессия закрывается (маршруты снимаются).
            if let Some(previous) = session.take() {
                tracing::info!(
                    session = previous,
                    "повторный Hello: закрываю прежнюю сессию"
                );
                close_session_routes(store, previous).await;
            }
            let fresh = store.lock().await.next_session();
            *session = Some(fresh);
            tracing::info!(daemon_pid, session = fresh, "сессия демона открыта");
            SvcReply::Ok
        }
        SvcRequest::Add {
            dest,
            gateway,
            iface_alias,
        } => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            if let Err(error) = validate_route(dest) {
                return validation_error(&error.to_string());
            }
            if let Err(message) = validate_iface_alias(&iface_alias) {
                return validation_error(&message);
            }
            // Только route_ps_args: ручное построение аргументов
            // запрещено (дисциплина инъекционной безопасности).
            if let Err(error) = run_ps(&route_ps_args(
                RouteCmd::Add,
                dest,
                Some(gateway),
                Some(&iface_alias),
            ))
            .await
            {
                return io_error(&error.to_string());
            }
            match store
                .lock()
                .await
                .add(session_id, dest, gateway, iface_alias)
            {
                Ok(()) => {
                    tracing::info!(dest = %dest, session = session_id, "маршрут добавлен");
                    SvcReply::Ok
                }
                Err(error) => io_error(&error.to_string()),
            }
        }
        SvcRequest::Del { dest } => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            if let Err(error) = validate_route(dest) {
                return validation_error(&error.to_string());
            }
            if !store.lock().await.contains(dest) {
                // Маршрута нет в live-set — работы нет (идемпотентность).
                tracing::warn!(%dest, "del маршрута, которого нет в live-set");
                return SvcReply::Ok;
            }
            if let Err(error) = run_ps(&route_ps_args(RouteCmd::Del, dest, None, None)).await {
                return io_error(&error.to_string());
            }
            match store.lock().await.del(session_id, dest) {
                Ok(_) => {
                    tracing::info!(dest = %dest, session = session_id, "маршрут удалён");
                    SvcReply::Ok
                }
                Err(error) => io_error(&error.to_string()),
            }
        }
        SvcRequest::Cleanup => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            // Снимаем ВСЕ live-маршруты: последовательный Remove по
            // каждому; проваленные остаются в live-set и попадают в ответ.
            let entries = store.lock().await.routes();
            let mut failures = Vec::new();
            for entry in entries {
                match run_ps(&route_ps_args(RouteCmd::Del, entry.dest, None, None)).await {
                    Ok(_) => {
                        if let Err(error) = store.lock().await.del(session_id, entry.dest) {
                            failures.push(format!("{}: {error}", entry.dest));
                        }
                    }
                    Err(error) => failures.push(format!("{}: {error}", entry.dest)),
                }
            }
            if failures.is_empty() {
                tracing::info!(session = session_id, "cleanup: live-маршруты сняты");
                SvcReply::Ok
            } else {
                io_error(&format!(
                    "не удалось удалить маршруты: {}",
                    failures.join("; ")
                ))
            }
        }
        SvcRequest::List => {
            if session.is_none() {
                return no_session_error();
            }
            SvcReply::RouteList {
                routes: store.lock().await.routes(),
            }
        }
    }
}

/// Отказ «сессия не открыта»: демон обязан начать с Hello.
fn no_session_error() -> SvcReply {
    validation_error("сессия не открыта: начните обмен с Hello")
}

/// Ответ-ошибка валидации (русский текст).
fn validation_error(message: &str) -> SvcReply {
    SvcReply::Error {
        code: svc_err::VALIDATION.to_string(),
        message: message.to_string(),
    }
}

/// Ответ-ошибка взаимодействия с ОС.
fn io_error(message: &str) -> SvcReply {
    SvcReply::Error {
        code: svc_err::IO.to_string(),
        message: message.to_string(),
    }
}

/// Закрывает сессию: снимает её маршруты из live-set с записью `del`
/// в журнал и best-effort удаляет их из ОС (ошибка удаления — warn:
/// остаток подберёт сверка при следующем старте службы).
async fn close_session_routes(store: &Mutex<RouteStore>, session: SessionId) {
    let removed = store.lock().await.close_session(session);
    if removed.is_empty() {
        return;
    }
    tracing::info!(
        session,
        routes = removed.len(),
        "сессия закрыта: снимаю её маршруты"
    );
    for entry in removed {
        match run_ps(&route_ps_args(RouteCmd::Del, entry.dest, None, None)).await {
            Ok(_) => tracing::info!(dest = %entry.dest, "маршрут умершей сессии удалён"),
            Err(error) => tracing::warn!(
                dest = %entry.dest,
                %error,
                "не удалось удалить маршрут умершей сессии (подберёт сверка при старте)"
            ),
        }
    }
}

// ----------------------------------------------------------------------
// Windows-транспорт: named pipe + LengthJsonCodec (паттерн daemon).
// ----------------------------------------------------------------------

#[cfg(windows)]
mod transport {
    use std::io;
    use std::sync::Arc;

    use bottle_core::frame::LengthJsonCodec;
    use bottle_core::svcrpc::{SvcReply, SvcRequest};
    use bytes::BytesMut;
    use tokio::io::{split, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::Mutex;
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Encoder, FramedRead};
    use tokio_util::sync::CancellationToken;

    use super::{close_session_routes, dispatch};
    use crate::journal::{RouteStore, SessionId};

    /// Обслуживает named pipe: по одному клиенту на соединение, свежий
    /// экземпляр канала на каждый цикл, до отмены `shutdown`.
    ///
    /// Ошибки отдельных соединений логируются и не останавливают
    /// сервер; наружу уходит только отказ создать экземпляр канала.
    pub async fn serve(
        pipe_name: &str,
        store: Arc<Mutex<RouteStore>>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let mut server = ServerOptions::new().create(pipe_name)?;
        tracing::info!(pipe = pipe_name, "pipe-сервер службы маршрутов запущен");
        loop {
            tokio::select! {
                connected = server.connect() => connected?,
                () = shutdown.cancelled() => {
                    tracing::info!("pipe-сервер службы маршрутов остановлен по отмене");
                    return Ok(());
                }
            }
            handle_connection(server, &store, &shutdown).await;
            server = ServerOptions::new().create(pipe_name)?;
        }
    }

    /// Обслуживает одно соединение демона: кадры запросов → ответы
    /// диспетчера. Завершается при отключении демона, некорректном
    /// кадре или отмене `shutdown`; в конце сессия закрывается —
    /// её маршруты убираются из ОС и журнала (демон умер).
    async fn handle_connection(
        pipe: NamedPipeServer,
        store: &Arc<Mutex<RouteStore>>,
        shutdown: &CancellationToken,
    ) {
        let mut session: Option<SessionId> = None;
        let (read_half, mut write_half) = split(pipe);
        let mut reader = FramedRead::new(read_half, LengthJsonCodec::<SvcRequest>::new());
        let mut reply_codec = LengthJsonCodec::<SvcReply>::new();

        loop {
            let req = tokio::select! {
                maybe = reader.next() => match maybe {
                    Some(Ok(req)) => req,
                    Some(Err(error)) => {
                        tracing::warn!(%error, "некорректный кадр от демона, соединение закрывается");
                        break;
                    }
                    None => {
                        tracing::info!("демон отключился");
                        break;
                    }
                },
                () = shutdown.cancelled() => break,
            };
            let reply = dispatch(store, &mut session, req).await;
            if let Err(error) = send_reply(&mut write_half, &mut reply_codec, reply).await {
                tracing::warn!(%error, "не удалось отправить ответ демону");
                break;
            }
        }

        // Соединение закрыто (демон умер/отключился либо служба
        // останавливается) — маршруты сессии не переживают (спека §9).
        if let Some(session) = session.take() {
            tracing::info!(session, "закрываю сессию демона");
            close_session_routes(store, session).await;
        }

        let pipe = reader.into_inner().unsplit(write_half);
        if let Err(error) = pipe.disconnect() {
            tracing::warn!(%error, "disconnect экземпляра канала не удался");
        }
    }

    /// Кодирует и отправляет ответ демону (fallible-путь `Encoder`).
    async fn send_reply<W>(
        write_half: &mut W,
        codec: &mut LengthJsonCodec<SvcReply>,
        reply: SvcReply,
    ) -> io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut buf = BytesMut::new();
        codec.encode(reply, &mut buf).map_err(io::Error::other)?;
        write_half.write_all(&buf).await
    }
}

#[cfg(windows)]
pub use transport::serve;

/// Не-Windows стаб транспорта: named pipe доступен только на Windows;
/// консольный режим dev-хоста ждёт сигнала остановки, чтобы журнал и
/// сверка (задача 5) оставались наблюдаемыми вне Windows.
#[cfg(not(windows))]
pub async fn serve(
    _pipe_name: &str,
    _store: std::sync::Arc<Mutex<RouteStore>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    tracing::warn!("named pipe доступен только на Windows — сервер ждёт сигнал остановки");
    shutdown.cancelled().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use bottle_core::svcrpc::SvcRequest;

    use super::*;
    use crate::journal::JOURNAL_FILE_NAME;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    fn add_req(dest: &str) -> SvcRequest {
        SvcRequest::Add {
            dest: ip(dest),
            gateway: ip("192.168.1.1"),
            iface_alias: "Ethernet".to_string(),
        }
    }

    /// Хранилище во временном каталоге под мьютексом (как в сервере).
    async fn test_store() -> (tempfile::TempDir, Mutex<RouteStore>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = RouteStore::open(&tmp.path().join(JOURNAL_FILE_NAME)).expect("журнал открылся");
        (tmp, Mutex::new(store))
    }

    #[test]
    fn alias_validation_rules() {
        assert_eq!(validate_iface_alias("Ethernet"), Ok(()));
        assert_eq!(validate_iface_alias("vEthernet (WSL)"), Ok(()));
        assert!(
            validate_iface_alias("").is_err(),
            "пустой алиас отвергается"
        );
        let too_long = "x".repeat(MAX_IFACE_ALIAS_LEN + 1);
        assert!(
            validate_iface_alias(&too_long).is_err(),
            "длиннее 64 символов"
        );
        assert!(
            validate_iface_alias("Ethernet\n2").is_err(),
            "управляющие символы"
        );
        assert!(
            validate_iface_alias("Eth\x00ernet").is_err(),
            "NUL в алиасе"
        );
    }

    #[tokio::test]
    async fn requests_without_hello_are_rejected() {
        let (_tmp, store) = test_store().await;
        let mut session = None;

        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        let SvcReply::Error { code, message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(code, svc_err::VALIDATION);
        assert!(
            message.contains("Hello"),
            "сообщение подсказывает Hello: {message}"
        );
        assert!(session.is_none(), "сессия не открыта");
    }

    #[tokio::test]
    async fn add_rejects_special_dest_before_any_os_call() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        // Loopback: валидация рулит ДО вызова PowerShell — на хосте
        // разработки run_ps вернул бы NotWindows, а не VALIDATION.
        let reply = dispatch(&store, &mut session, add_req("127.0.0.1")).await;
        let SvcReply::Error { code, message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(code, svc_err::VALIDATION);
        assert!(
            message.contains("loopback"),
            "текст ошибки валидации: {message}"
        );
        assert!(
            store.lock().await.routes().is_empty(),
            "отказ валидации не трогает live-set"
        );
    }

    #[tokio::test]
    async fn add_rejects_invalid_alias() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        for alias in ["", &"y".repeat(65), "Bad\nAlias"] {
            let reply = dispatch(
                &store,
                &mut session,
                SvcRequest::Add {
                    dest: ip("203.0.113.10"),
                    gateway: ip("192.168.1.1"),
                    iface_alias: alias.to_string(),
                },
            )
            .await;
            assert!(
                matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::VALIDATION),
                "алиас {alias:?} должен отвергаться с VALIDATION, получили {reply:?}"
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn add_valid_request_reports_io_on_non_windows() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        // Валидный запрос дошёл до run_ps, но вне Windows операции над
        // маршрутами недоступны — Error IO с русским текстом.
        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        assert!(
            matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::IO),
            "ожидали Error IO, получили {reply:?}"
        );
        assert!(
            store.lock().await.routes().is_empty(),
            "провал ОС-операции не меняет live-set"
        );
    }

    #[tokio::test]
    async fn del_of_untracked_dest_is_idempotent_ok() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        let reply = dispatch(
            &store,
            &mut session,
            SvcRequest::Del {
                dest: ip("203.0.113.99"),
            },
        )
        .await;
        assert_eq!(reply, SvcReply::Ok, "нет маршрута — нет работы");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn del_tracked_dest_reports_io_failure_and_keeps_state() {
        let (_tmp, store) = test_store().await;
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
        }
        let mut session = Some(1);

        let reply = dispatch(
            &store,
            &mut session,
            SvcRequest::Del {
                dest: ip("203.0.113.10"),
            },
        )
        .await;
        assert!(
            matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::IO),
            "ожидали Error IO вне Windows, получили {reply:?}"
        );
        assert_eq!(
            store.lock().await.routes().len(),
            1,
            "провал удаления из ОС оставляет маршрут в live-set"
        );
    }

    #[tokio::test]
    async fn hello_opens_session_and_rehello_closes_previous() {
        let (_tmp, store) = test_store().await;
        // Маршруты «прошлой» сессии уже в журнале (как после сбоя).
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
        }
        let mut session = None;

        // Первый Hello: прежних живых сессий нет, открывается новая.
        let reply = dispatch(&store, &mut session, SvcRequest::Hello { daemon_pid: 4242 }).await;
        assert_eq!(reply, SvcReply::Ok);
        let first = session.expect("сессия открыта");

        // Повторный Hello на живом соединении — перезапуск сессии:
        // маршруты прежней снимаются (журнал + live-set).
        store
            .lock()
            .await
            .add(first, ip("203.0.113.11"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        let reply = dispatch(&store, &mut session, SvcRequest::Hello { daemon_pid: 4242 }).await;
        assert_eq!(reply, SvcReply::Ok);
        let second = session.expect("новая сессия открыта");
        assert_ne!(first, second, "сессии монотонно нумеруются");

        let routes = store.lock().await.routes();
        assert!(
            routes.iter().all(|entry| entry.session == second),
            "после перезапуска сессии live-set содержит только её маршруты: {routes:?}"
        );
    }

    #[tokio::test]
    async fn list_returns_live_set_snapshot() {
        let (_tmp, store) = test_store().await;
        store
            .lock()
            .await
            .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        let mut session = Some(1);

        let reply = dispatch(&store, &mut session, SvcRequest::List).await;
        let SvcReply::RouteList { routes } = reply else {
            panic!("ожидали RouteList, получили {reply:?}");
        };
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].dest, ip("203.0.113.10"));
        assert_eq!(routes[0].session, 1);
    }

    #[tokio::test]
    async fn cleanup_on_empty_live_set_is_ok() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        let reply = dispatch(&store, &mut session, SvcRequest::Cleanup).await;
        assert_eq!(reply, SvcReply::Ok, "чистый live-set — немедленный успех");
    }
}
