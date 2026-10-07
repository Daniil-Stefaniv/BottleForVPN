//! Тесты UDS-сервера демона (M4 Task 7): реальный круг
//! сервер+клиент на unix, права сокета 0600 (Review Focus #2),
//! пережатие stale-сокета, параллельные клиенты.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use bottle_core::ipc::{daemon_socket_path, uds_exchange, CliReply, CliRequest};
use tokio_util::sync::CancellationToken;

use crate::Daemon;

/// Демон без менеджера: List/Show работают, start — переходная ошибка.
fn daemon_with_store(root: &std::path::Path) -> Arc<Daemon> {
    let store = crate::store::Store::open(root).unwrap();
    store
        .save(&host_profile("corp-vpn"))
        .expect("профиль сохранён");
    Arc::new(Daemon {
        store,
        manager: None,
        wsl: None,
        rootfs: None,
    })
}

fn host_profile(id: &str) -> bottle_core::profile::BottleProfile {
    use bottle_core::profile::*;
    BottleProfile {
        id: id.to_string(),
        name: format!("Хост {id}"),
        kind: BottleKind::Host,
        vpn: VpnConfig::Host {
            client: r"C:\vpn.exe".to_string(),
            args: None,
            endpoints: vec!["vpn.example.com".to_string()],
            interface: None,
        },
        expose: ExposeConfig {
            socks5: 1080,
            forwards: vec![],
            dns: None,
            launch: None,
        },
    }
}

/// Права файла сокета (unix-режим без типа файла).
fn socket_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn uds_serves_list_over_real_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = daemon_with_store(tmp.path());
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
        &CliRequest::List,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("обмен по UDS");

    let CliReply::List { bottles } = reply else {
        panic!("ожидали List, получили {reply:?}");
    };
    assert_eq!(bottles.len(), 1);
    assert_eq!(bottles[0].id, "corp-vpn");

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Review Focus #2: сокет 0600 — чужой пользователь машины не подключится.
#[tokio::test]
async fn uds_socket_has_owner_only_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = daemon_with_store(tmp.path());
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

    let daemon = daemon_with_store(tmp.path());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn({
        let socket = socket.clone();
        let shutdown = shutdown.clone();
        async move { crate::uds::serve(&socket, daemon, shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reply = uds_exchange(
        &socket,
        &CliRequest::List,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("stale-файл пережат, обмен прошёл");
    assert!(matches!(reply, CliReply::List { .. }));

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}

/// Параллельные клиенты (T13): второй получает ответ, не дожидаясь
/// закрытия соединения первого.
#[tokio::test]
async fn uds_second_serve_on_live_socket_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = daemon_with_store(tmp.path());
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
            daemon_with_store(tmp.path()),
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
        &CliRequest::List,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("живой демон отвечает после попытки перебинда");
    assert!(matches!(reply, CliReply::List { .. }));

    shutdown.cancel();
    first.await.unwrap().expect("serve завершился без ошибки");
}

#[tokio::test]
async fn uds_serves_parallel_clients() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = daemon_socket_path(tmp.path());
    let daemon = daemon_with_store(tmp.path());
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
                &CliRequest::List,
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
            &CliRequest::List,
            Duration::from_secs(2),
            Duration::from_secs(2),
        ),
    )
    .await
    .expect("второй клиент обслужен параллельно с первым")
    .expect("обмен второго клиента");
    assert!(matches!(second_reply, CliReply::List { .. }));
    assert!(first.await.is_ok());

    shutdown.cancel();
    server.await.unwrap().expect("serve завершился без ошибки");
}
