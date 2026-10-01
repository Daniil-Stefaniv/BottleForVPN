//! Подкоманда `relay`: SOCKS5- и forward-серверы внутри namespace бутылки.
//!
//! Запускается самим агентом через `ip netns exec` (см.
//! [`crate::netns::netns_up`]) и слушает `0.0.0.0` внутри namespace:
//! SOCKS5 на свой порт и по forward-слушателю на каждый проброс.
//! Доступ снаружи обеспечивают мосты в default namespace.

use std::io;
use std::str::FromStr;

use bottle_core::protocol::ForwardPort;
use relay_lib::forward::{serve_forward, TargetAddr};
use relay_lib::socks5::serve_socks5;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

/// Запускает relay: SOCKS5 на `0.0.0.0:socks5_port` и по forward-серверу
/// на `0.0.0.0:<port>` для каждого проброса.
///
/// Слушатели занимаются до старта серверов, а цели пробросов разбираются
/// заранее: отказ (занятый порт, некорректная цель) происходит до того,
/// как relay начнёт обслуживать соединения. Серверы работают
/// конкурентно; ошибка возвращается при первой фатальной ошибке любого
/// из них (отказ слушателя `accept`) — дальше relay работать не может.
pub async fn run_relay(socks5_port: u16, forwards: Vec<ForwardPort>) -> io::Result<()> {
    let mut servers: JoinSet<io::Result<()>> = JoinSet::new();

    let socks5_listener = TcpListener::bind(("0.0.0.0", socks5_port)).await?;
    servers.spawn(async move { serve_socks5(socks5_listener).await });

    for forward in forwards {
        let target = parse_target(&forward.target)?;
        let listener = TcpListener::bind(("0.0.0.0", forward.port)).await?;
        servers.spawn(async move { serve_forward(listener, target).await });
    }

    // Серверы — бесконечные циклы accept'а: любой завершившийся остановил
    // слушатель, остальные сносятся вместе с процессом relay.
    match servers.join_next().await {
        Some(Ok(Ok(()))) => Err(io::Error::other(
            "relay-сервер остановился без ошибки (недостижимо)",
        )),
        Some(Ok(Err(error))) => Err(error),
        Some(Err(join_error)) => {
            if join_error.is_panic() {
                std::panic::resume_unwind(join_error.into_panic());
            }
            Err(io::Error::other("relay-задача была отменена"))
        }
        None => Err(io::Error::other("все relay-серверы завершились")),
    }
}

/// Разбирает цель проброса `host:port` в [`TargetAddr`].
fn parse_target(target: &str) -> io::Result<TargetAddr> {
    TargetAddr::from_str(target)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Разбирает аргумент `--forward ПОРТ:ЦЕЛЬ` (например,
/// `3389:10.20.30.15:3389`) в [`ForwardPort`].
///
/// Строка режется по первому двоеточию: порт слушателя не содержит
/// двоеточий, а цель может быть IPv6-литералом в скобках. Цель
/// дополнительно проверяется разбором в [`TargetAddr`], чтобы relay
/// не падал уже после запуска.
pub fn parse_forward_spec(spec: &str) -> Result<ForwardPort, String> {
    let (port, target) = spec.split_once(':').ok_or_else(|| {
        format!("ожидается ПОРТ:ЦЕЛЬ (например, 3389:10.20.30.15:3389): {spec:?}")
    })?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("порт слушателя не число 1..=65535: {port:?}"))?;
    if port == 0 {
        return Err(format!("порт слушателя не число 1..=65535: {port:?}"));
    }
    parse_target(target).map_err(|e| format!("некорректная цель {target:?}: {e}"))?;
    Ok(ForwardPort {
        port,
        target: target.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forward_spec_accepts_ipv4_target() {
        let parsed = parse_forward_spec("3389:10.20.30.15:3389").expect("валидная спецификация");
        assert_eq!(parsed.port, 3389);
        assert_eq!(parsed.target, "10.20.30.15:3389");
    }

    #[test]
    fn parse_forward_spec_accepts_domain_target() {
        let parsed =
            parse_forward_spec("8443:corp.example.com:443").expect("валидная спецификация");
        assert_eq!(parsed.port, 8443);
        assert_eq!(parsed.target, "corp.example.com:443");
    }

    #[test]
    fn parse_forward_spec_accepts_bracketed_ipv6_target() {
        let parsed = parse_forward_spec("53:[2001:db8::1]:53").expect("валидная спецификация");
        assert_eq!(parsed.port, 53);
        assert_eq!(parsed.target, "[2001:db8::1]:53");
    }

    #[test]
    fn parse_forward_spec_rejects_missing_separator() {
        assert!(parse_forward_spec("3389").is_err());
        assert!(parse_forward_spec("").is_err());
    }

    #[test]
    fn parse_forward_spec_rejects_bad_listener_port() {
        assert!(parse_forward_spec("0:10.20.30.15:3389").is_err());
        assert!(parse_forward_spec("65536:10.20.30.15:3389").is_err());
        assert!(parse_forward_spec("rdp:10.20.30.15:3389").is_err());
    }

    #[test]
    fn parse_forward_spec_rejects_bad_target() {
        assert!(parse_forward_spec("3389:").is_err());
        assert!(parse_forward_spec("3389:nohost").is_err());
        assert!(parse_forward_spec("3389:host:notaport").is_err());
    }

    #[tokio::test]
    // Гард держится через await намеренно: мьютекс сериализует тесты
    // друг с другом (каждый в своём рантайме/потоке), внутри одного
    // теста конкурентов на локе нет.
    #[allow(clippy::await_holding_lock)]
    async fn run_relay_serves_socks5_and_stays_alive() {
        // Эфемерные порты контестируются с другими тестами крейта —
        // см. test_support::EPHEMERAL_PORT_LOCK.
        let _ports = crate::test_support::EPHEMERAL_PORT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Свободные порты: bind и drop слушателей.
        let free_port = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .expect("bind ephemeral listener")
                .port()
        };
        let socks5_port = free_port();

        let relay = tokio::spawn(run_relay(socks5_port, vec![]));

        // Моста нет — relay доступен по 127.0.0.1. Задача relay ещё могла
        // не дойти до bind, поэтому подключение повторяется до успеха.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut connected = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", socks5_port)).await {
                Ok(stream) => break stream,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(error) => panic!("подключение к relay: {error}"),
            }
        };

        // SOCKS5-согласование проходит, сервер продолжает работать.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        connected
            .write_all(&[0x05, 0x01, 0x00])
            .await
            .expect("send");
        let mut method = [0u8; 2];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connected.read_exact(&mut method),
        )
        .await
        .expect("ответ в пределах таймаута")
        .expect("прочитать ответ");
        assert_eq!(&method, &[0x05, 0x00], "выбран метод без аутентификации");

        assert!(!relay.is_finished(), "relay продолжает работать");
        relay.abort();
    }

    #[tokio::test]
    // Гард держится через await намеренно: мьютекс сериализует тесты
    // друг с другом (каждый в своём рантайме/потоке), внутри одного
    // теста конкурентов на локе нет.
    #[allow(clippy::await_holding_lock)]
    async fn run_relay_fails_on_busy_port() {
        // Эфемерные порты контестируются с другими тестами крейта —
        // см. test_support::EPHEMERAL_PORT_LOCK.
        let _ports = crate::test_support::EPHEMERAL_PORT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Занимаем порт слушателем на том же wildcard-адресе, что и relay
        // (конфликт wildcard/specific на BSD с SO_REUSEADDR не возникает).
        let squatter = TcpListener::bind(("0.0.0.0", 0)).await.expect("bind");
        let busy_port = squatter.local_addr().expect("addr").port();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_relay(busy_port, vec![]),
        )
        .await
        .expect("bind должен упасть быстро, а не зависать");
        let error = outcome.expect_err("порт занят, bind должен упасть");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::AddrInUse,
            "ожидалась ошибка занятого порта"
        );
    }
}
