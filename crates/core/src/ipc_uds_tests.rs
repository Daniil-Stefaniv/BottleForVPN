//! Тесты UDS-транспорта CLI↔демона (M4 Task 7): клиентский обмен
//! кадрами LengthJsonCodec, retry-семантика «демон не запущен»,
//! путь сокета в каталоге данных. Реальный круг на unix; на Windows
//! файл не компилируется (клиент — named pipe).

#![cfg(unix)]

use std::time::Duration;

use crate::frame::encode_frame;
use crate::ipc::{daemon_socket_path, uds_exchange, CliReply, CliRequest, DAEMON_SOCK_FILE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// Мини-сервер: читает один кадр запроса, отвечает фикс. ответом.
async fn echo_server(listener: UnixListener, reply: CliReply) {
    let (mut stream, _) = listener.accept().await.expect("клиент подключился");
    // Запрос читаем «сколько есть» — содержимое не важно, клиент ждёт
    // только ответа; важно не потерять ни байта записи ответа.
    let mut chunk = [0u8; 4096];
    let _read = stream.read(&mut chunk).await.expect("запрос прочитан");
    let frame = encode_frame(&reply);
    stream.write_all(&frame).await.expect("ответ отправлен");
}

#[tokio::test]
async fn uds_exchange_round_trips_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let server = tokio::spawn(echo_server(
        listener,
        CliReply::Started {
            id: "office-a".to_string(),
        },
    ));

    let reply = uds_exchange(
        &path,
        &CliRequest::Doctor,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
    .await
    .expect("обмен прошёл");

    assert_eq!(
        reply,
        CliReply::Started {
            id: "office-a".to_string()
        }
    );
    server.await.unwrap();
}

/// Сокета нет вообще: короткое retry-окно → «демон не запущен»
/// (ECONNREFUSED/ENOENT — то же сообщение, что у named pipe).
#[tokio::test]
async fn uds_exchange_without_server_is_daemon_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.sock");
    let error = uds_exchange(
        &path,
        &CliRequest::Doctor,
        Duration::from_secs(5),
        Duration::from_millis(80),
    )
    .await
    .expect_err("демона нет");
    assert!(
        error.contains("запущен ли pogrebok-daemon"),
        "понятная ошибка: {error}"
    );
}

/// Сервер принимает, но молчит: клиент отрезается по таймауту ответа.
#[tokio::test]
async fn uds_exchange_times_out_on_silent_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let server = tokio::spawn(async move {
        // Принимаем и держим соединение, ничего не отвечая.
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let started = std::time::Instant::now();
    let error = uds_exchange(
        &path,
        &CliRequest::Doctor,
        Duration::from_millis(300),
        Duration::from_millis(50),
    )
    .await
    .expect_err("сервер молчит");
    assert!(error.contains("не ответил"), "ошибка таймаута: {error}");
    assert!(started.elapsed() < Duration::from_secs(3));
    server.abort();
}

/// Сервер появляется с задержкой — retry-окно подключения пережидает
/// стык (демон поднимается).
#[tokio::test]
async fn uds_exchange_retries_until_server_appears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.sock");
    let late_path = path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let listener = UnixListener::bind(&late_path).expect("bind");
        echo_server(listener, CliReply::Done { warning: None }).await;
    });

    let reply = uds_exchange(
        &path,
        &CliRequest::RoutesCleanup,
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .await
    .expect("retry-окно переждало старт сервера");
    assert_eq!(reply, CliReply::Done { warning: None });
}

/// Путь сокета — `<data>/daemon.sock` (контракт D3/Global Constraints).
#[test]
fn daemon_socket_path_is_data_dir_child() {
    let path = daemon_socket_path(std::path::Path::new("/data"));
    assert_eq!(
        path,
        std::path::PathBuf::from("/data").join(DAEMON_SOCK_FILE)
    );
    assert_eq!(DAEMON_SOCK_FILE, "daemon.sock");
}
