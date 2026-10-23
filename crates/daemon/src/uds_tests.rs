//! Тесты UDS-сервера демона (M4 Task 7): реальный круг
//! сервер+клиент на unix, права сокета 0600 (Review Focus #2),
//! пережатие stale-сокета, параллельные клиенты.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use bottle_core::frame::LengthJsonCodec;
use bottle_core::ipc::{daemon_socket_path, uds_exchange, CliReply, CliRequest, GuiRequest};
use tokio_util::sync::CancellationToken;

use crate::Daemon;

/// Демон 0.3 для тестов: реестр, токен, без менеджера.
fn gui_daemon(root: &std::path::Path, token: &str) -> Arc<Daemon> {
    crate::test_daemon(root, token, None)
}

/// Права файла сокета (unix-режим без типа файла).
fn socket_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Служебная команда (Doctor) проходит реальный круг по UDS: без
/// менеджера демон отвечает понятной ошибкой, а не молчанием.
#[tokio::test]
async fn uds_serves_service_command_over_real_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();

    let server_socket = socket.clone();
    let server_shutdown = shutdown.clone();
    let server =
        tokio::spawn(
            async move { crate::uds::serve(&server_socket, daemon, server_shutdown).await },
        );
    // Даём серверу забиндиться.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reply = uds_exchange(
        &socket,
        &CliRequest::Doctor,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("обмен по UDS");

    let CliReply::Error { message } = reply else {
        panic!("ожидали Error без менеджера, получили {reply:?}");
    };
    assert!(message.contains("менеджер"), "понятная ошибка: {message}");

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Review Focus #2: сокет 0600 — чужой пользователь машины не подключится.
#[tokio::test]
async fn uds_socket_has_owner_only_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();

    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        socket_mode(&socket),
        0o600,
        "права сокета — только владелец"
    );

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Перезапуск демона: старый (мёртвый) файл сокета не мешает bind.
#[tokio::test]
async fn uds_serve_replaces_stale_socket_file() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    // Имитация хвоста от упавшего демона.
    std::fs::write(&socket, b"stale").unwrap();

    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reply = uds_exchange(
        &socket,
        &CliRequest::Doctor,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("stale-файл пережат, обмен прошёл");
    assert!(matches!(reply, CliReply::Error { .. }), "Doctor без менеджера: {reply:?}");

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Параллельные клиенты (T13): второй получает ответ, не дожидаясь
/// закрытия соединения первого.
#[tokio::test]
async fn uds_second_serve_on_live_socket_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();

    let first = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Второй сервер на тот же путь: ЖИВОЙ первый должен защитить сокет.
    let second_error = tokio::time::timeout(
        Duration::from_secs(3),
        crate::uds::serve(
            &socket,
            gui_daemon(tmp.path(), "tok1"),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("живой демон защищает сокет (serve возвращается, а не слушает)")
    .expect_err("живой демон защищает свой сокет");
    assert!(
        second_error.to_string().contains("уже запущен"),
        "понятная ошибка: {second_error}"
    );

    // Первый демон продолжает отвечать (сокет не тронут).
    let reply = uds_exchange(
        &socket,
        &CliRequest::Doctor,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("живой демон отвечает после попытки перебинда");
    assert!(matches!(reply, CliReply::Error { .. }), "Doctor без менеджера: {reply:?}");

    shutdown.cancel();
    first.await.unwrap().expect("serve завершился без ошибки");
}

#[tokio::test]
async fn uds_serves_parallel_clients() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();

    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Первый клиент держит соединение открытым (не читая ответ сразу).
    let first = tokio::spawn({
        let socket = socket.clone();
        async move {
            uds_exchange(
                &socket,
                &CliRequest::Doctor,
                Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Второй клиент получает ответ при живом первом соединении.
    let second_reply = tokio::time::timeout(
        Duration::from_secs(3),
        uds_exchange(
            &socket,
            &CliRequest::Doctor,
            Duration::from_secs(2),
            Duration::from_secs(2),
        ),
    )
    .await
    .expect("второй клиент обслужен параллельно с первым")
    .expect("обмен второго клиента");
    assert!(matches!(second_reply, CliReply::Error { .. }), "Doctor без менеджера: {second_reply:?}");
    assert!(first.await.is_ok());

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}


// ---------------------------------------------------------------------
// Протокол 0.3: GUI↔демон поверх UDS — auth-токен, Sync, Subscribe.
// ---------------------------------------------------------------------

/// Клиент протокола GUI: кадры serde_json::Value поверх UDS (та же
/// проводная форма, что у control/event-соединений демона 0.3).
struct GuiClient {
    reader: tokio_util::codec::FramedRead<
        tokio::net::unix::OwnedReadHalf,
        LengthJsonCodec<serde_json::Value>,
    >,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl GuiClient {
    async fn connect(socket: &std::path::Path) -> GuiClient {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .expect("подключение к uds");
        let (read_half, write_half) = stream.into_split();
        GuiClient {
            reader: tokio_util::codec::FramedRead::new(read_half, LengthJsonCodec::new()),
            writer: write_half,
        }
    }

    /// Отправляет запрос кадром (сериализация в Value — нейтральный
    /// промежуточный формат сессии).
    async fn send<T: serde::Serialize>(&mut self, req: &T) {
        use tokio::io::AsyncWriteExt;
        use tokio_util::codec::Encoder;
        let value = serde_json::to_value(req).expect("запрос в Value");
        let mut buf = bytes::BytesMut::new();
        LengthJsonCodec::<serde_json::Value>::new()
            .encode(value, &mut buf)
            .expect("кадр кодируется");
        self.writer.write_all(&buf).await.expect("кадр отправлен");
    }

    /// Читает следующий кадр; `None` — сервер закрыл соединение.
    async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Option<T> {
        use tokio_stream::StreamExt;
        let value = tokio::time::timeout(Duration::from_secs(5), self.reader.next())
            .await
            .expect("кадр в пределах тайм-аута")?
            .expect("кадр разбирается");
        Some(serde_json::from_value(value).expect("кадр целевого типа"))
    }
}

/// Auth-гейт (спека §5): до Hello с верным токеном control-соединение
/// принимает только служебные Doctor/LogsExport; StartBottle отклоняется
/// с кодом AUTH; неверный токен — ошибка и разрыв; верный — Ok, и
/// дальше SyncProfiles принимается.
#[tokio::test]
async fn control_requires_hello_and_rejects_bad_token() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Запрос до Hello — отказ с кодом AUTH.
    let mut client = GuiClient::connect(&socket).await;
    client
        .send(&GuiRequest::StartBottle {
            id: "office-a".to_string(),
        })
        .await;
    let reply: bottle_core::ipc::GuiReply = client.recv().await.expect("ответ есть");
    let bottle_core::ipc::GuiReply::Error { message } = reply else {
        panic!("ожидали Error, получили {reply:?}");
    };
    assert!(message.starts_with("AUTH"), "код AUTH: {message}");

    // Неверный токен — ошибка и закрытие соединения.
    client
        .send(&GuiRequest::Hello {
            token: "bad".to_string(),
        })
        .await;
    let reply: bottle_core::ipc::GuiReply = client.recv().await.expect("ответ на Hello");
    assert!(
        matches!(&reply, bottle_core::ipc::GuiReply::Error { message } if message.starts_with("AUTH")),
        "неверный токен — AUTH-ошибка: {reply:?}"
    );
    let closed: Option<bottle_core::ipc::GuiReply> = client.recv().await;
    assert!(closed.is_none(), "соединение закрыто после неверного токена");

    // Верный токен — Ok; SyncProfiles принимается.
    let mut client = GuiClient::connect(&socket).await;
    client
        .send(&GuiRequest::Hello {
            token: "tok1".to_string(),
        })
        .await;
    let reply: bottle_core::ipc::GuiReply = client.recv().await.expect("Hello ок");
    assert_eq!(reply, bottle_core::ipc::GuiReply::Ok);
    client
        .send(&GuiRequest::SyncProfiles { profiles: vec![] })
        .await;
    let reply: bottle_core::ipc::GuiReply = client.recv().await.expect("Sync ок");
    assert_eq!(reply, bottle_core::ipc::GuiReply::Ok);

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Subscribe (спека §5): отдельное соединение с токеном становится
/// event-потоком — кадры DaemonEvent из broadcast-канала демона;
/// неверный токен — AUTH-ошибка и разрыв.
#[tokio::test]
async fn subscribe_streams_state_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = gui_daemon(tmp.path(), "tok1");
    let events = daemon.events.clone();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Неверный токен подписки — отказ и разрыв.
    let mut bad = GuiClient::connect(&socket).await;
    bad.send(&GuiRequest::Subscribe {
        token: "bad".to_string(),
    })
    .await;
    let reply: bottle_core::ipc::GuiReply = bad.recv().await.expect("ответ на Subscribe");
    assert!(
        matches!(&reply, bottle_core::ipc::GuiReply::Error { message } if message.starts_with("AUTH")),
        "Subscribe с чужим токеном — AUTH: {reply:?}"
    );
    let closed: Option<bottle_core::ipc::GuiReply> = bad.recv().await;
    assert!(closed.is_none(), "соединение подписки закрыто");

    // Верный токен: ACK Ok, дальше — поток событий.
    let mut subscriber = GuiClient::connect(&socket).await;
    subscriber
        .send(&GuiRequest::Subscribe {
            token: "tok1".to_string(),
        })
        .await;
    let ack: bottle_core::ipc::GuiReply = subscriber.recv().await.expect("ACK подписки");
    assert_eq!(ack, bottle_core::ipc::GuiReply::Ok);

    events
        .send(bottle_core::ipc::DaemonEvent::StateChanged {
            id: "office-a".to_string(),
            state: "up".to_string(),
            detail: None,
            socks5: Some(51820),
            forwards: vec![],
        })
        .expect("подписчик жив");
    let event: bottle_core::ipc::DaemonEvent = subscriber.recv().await.expect("событие пришло");
    assert_eq!(
        event,
        bottle_core::ipc::DaemonEvent::StateChanged {
            id: "office-a".to_string(),
            state: "up".to_string(),
            detail: None,
            socks5: Some(51820),
            forwards: vec![],
        }
    );

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

