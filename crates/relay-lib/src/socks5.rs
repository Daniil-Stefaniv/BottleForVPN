//! SOCKS5-сервер (RFC 1928): только CONNECT без аутентификации.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::copy::copy_bidirectional;
use crate::forward::{resolve_target, TargetAddr};

/// Версия протокола SOCKS5.
const SOCKS_VERSION: u8 = 0x05;
/// Единственный поддерживаемый метод аутентификации — без неё.
const METHOD_NO_AUTH: u8 = 0x00;
/// Ответ на согласование: ни один из предложенных методов не поддерживается.
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
/// Команда CONNECT.
const CMD_CONNECT: u8 = 0x01;
/// Тип адреса: IPv4.
const ATYP_IPV4: u8 = 0x01;
/// Тип адреса: доменное имя с 1-байтовой длиной.
const ATYP_DOMAIN: u8 = 0x03;
/// Тип адреса: IPv6.
const ATYP_IPV6: u8 = 0x04;

/// REP: запрос выполнен.
const REP_SUCCEEDED: u8 = 0x00;
/// REP: общий сбой SOCKS-сервера.
const REP_GENERAL_FAILURE: u8 = 0x01;
/// REP: хост недоступен (в т.ч. неразрешимое имя).
const REP_HOST_UNREACHABLE: u8 = 0x04;
/// REP: соединение с целью отклонено.
const REP_CONNECTION_REFUSED: u8 = 0x05;
/// REP: команда не поддерживается (BIND, UDP ASSOCIATE).
const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
/// REP: тип адреса не поддерживается.
const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// Разобранный запрос клиента.
enum Request {
    /// CONNECT на указанный адрес цели.
    Connect(TargetAddr),
    /// Команда отличается от CONNECT (BIND, UDP ASSOCIATE, неизвестные).
    UnsupportedCommand,
    /// Тип адреса не поддерживается.
    UnsupportedAddressType,
}

/// Сервер SOCKS5: принимает соединения на `listener` и обслуживает
/// запросы CONNECT (RFC 1928; поддерживается только метод без
/// аутентификации).
///
/// Соединения обслуживаются конкурентно (`tokio::spawn`). Отказ
/// отдельного соединения (некорректный протокол, отказ цели) закрывает
/// только его и логируется через `tracing`, не останавливая сервер.
/// Логируются только метаданные (адреса, коды ответов), без payload.
/// Ошибка возвращается только при отказе самого слушателя (`accept`).
pub async fn serve_socks5(listener: TcpListener) -> io::Result<()> {
    loop {
        let (client, peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = handle_connection(client, peer).await {
                tracing::warn!(%peer, %error, "socks5 connection failed");
            }
        });
    }
}

/// Обслуживает одно соединение: согласование метода, затем цикл запросов.
///
/// После ответа с ошибкой (отказ диала, неподдерживаемая команда или тип
/// адреса) соединение не закрывается — клиент может послать следующий
/// запрос. Закрывается соединение только при некорректном протоколе
/// (чужая версия, обрезанные байты) или после завершения копирования
/// успешного CONNECT.
async fn handle_connection(mut client: TcpStream, peer: SocketAddr) -> io::Result<()> {
    client.set_nodelay(true)?;
    negotiate_method(&mut client).await?;
    loop {
        let target = match read_request(&mut client).await? {
            Request::Connect(target) => target,
            Request::UnsupportedCommand => {
                send_reply(&mut client, REP_COMMAND_NOT_SUPPORTED).await?;
                continue;
            }
            Request::UnsupportedAddressType => {
                send_reply(&mut client, REP_ADDRESS_TYPE_NOT_SUPPORTED).await?;
                continue;
            }
        };

        let addr = match resolve_target(&target).await {
            Ok(addr) => addr,
            Err(error) => {
                tracing::debug!(%peer, ?target, %error, "socks5 target resolve failed");
                send_reply(&mut client, REP_HOST_UNREACHABLE).await?;
                continue;
            }
        };

        let upstream = match TcpStream::connect(addr).await {
            Ok(upstream) => upstream,
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                tracing::debug!(%peer, target = %addr, %error, "socks5 target refused");
                send_reply(&mut client, REP_CONNECTION_REFUSED).await?;
                continue;
            }
            Err(error) => {
                tracing::debug!(%peer, target = %addr, %error, "socks5 target dial failed");
                send_reply(&mut client, REP_GENERAL_FAILURE).await?;
                continue;
            }
        };
        upstream.set_nodelay(true)?;

        send_reply(&mut client, REP_SUCCEEDED).await?;
        tracing::debug!(%peer, target = %addr, "socks5 connection established");
        copy_bidirectional(client, upstream).await?;
        tracing::debug!(%peer, target = %addr, "socks5 connection closed");
        return Ok(());
    }
}

/// Согласование метода аутентификации (RFC 1928, раздел 3).
///
/// Сервер предлагает только «без аутентификации» (`0x00`); если клиент
/// его не предложил — отвечает `0xFF` и закрывает соединение.
async fn negotiate_method(client: &mut TcpStream) -> io::Result<()> {
    let mut header = [0u8; 2]; // VER, NMETHODS
    client.read_exact(&mut header).await?;
    if header[0] != SOCKS_VERSION {
        return Err(protocol_error(format!(
            "unsupported method negotiation version {:#04x}",
            header[0]
        )));
    }
    let mut methods = vec![0u8; header[1] as usize];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NO_AUTH) {
        client
            .write_all(&[SOCKS_VERSION, METHOD_NO_ACCEPTABLE])
            .await?;
        return Err(protocol_error("client offered no acceptable auth method"));
    }
    client.write_all(&[SOCKS_VERSION, METHOD_NO_AUTH]).await
}

/// Читает и разбирает запрос клиента (RFC 1928, раздел 4).
async fn read_request(client: &mut TcpStream) -> io::Result<Request> {
    let mut head = [0u8; 4]; // VER, CMD, RSV, ATYP
    client.read_exact(&mut head).await?;
    if head[0] != SOCKS_VERSION {
        return Err(protocol_error(format!(
            "unsupported request version {:#04x}",
            head[0]
        )));
    }
    if head[1] != CMD_CONNECT {
        return Ok(Request::UnsupportedCommand);
    }

    let mut port = [0u8; 2];
    let target = match head[3] {
        ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            client.read_exact(&mut octets).await?;
            client.read_exact(&mut port).await?;
            TargetAddr::Ip(SocketAddr::from((octets, u16::from_be_bytes(port))))
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut host = vec![0u8; len[0] as usize];
            client.read_exact(&mut host).await?;
            client.read_exact(&mut port).await?;
            let host = String::from_utf8(host)
                .map_err(|_| protocol_error("domain address is not valid UTF-8"))?;
            TargetAddr::Domain {
                host,
                port: u16::from_be_bytes(port),
            }
        }
        ATYP_IPV6 => {
            let mut octets = [0u8; 16];
            client.read_exact(&mut octets).await?;
            client.read_exact(&mut port).await?;
            TargetAddr::Ip(SocketAddr::from((
                std::net::Ipv6Addr::from(octets),
                u16::from_be_bytes(port),
            )))
        }
        _ => return Ok(Request::UnsupportedAddressType),
    };
    Ok(Request::Connect(target))
}

/// Отправляет ответ сервера: VER=5, REP, RSV=0, ATYP=IPv4,
/// BND.ADDR=0.0.0.0, BND.PORT=0 (RFC 1928 допускает такие BND-поля).
async fn send_reply(client: &mut TcpStream, reply: u8) -> io::Result<()> {
    client
        .write_all(&[SOCKS_VERSION, reply, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
}

/// Ошибка протокола SOCKS5: соединение закрывается, инцидент логируется.
fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Верхний предел для любой сетевой операции в тестах.
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Эхо-сервер: слушает loopback на свободном порту (оба стека —
    /// `127.0.0.1` и `::1`, т.к. `localhost` резолвится в любой из них),
    /// возвращает те же байты. Возвращает IPv4-адрес слушателя.
    async fn spawn_echo() -> io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let v6 = TcpListener::bind(SocketAddr::new(
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            addr.port(),
        ))
        .await;
        if let Ok(v6) = v6 {
            tokio::spawn(echo_connections(v6));
        }
        tokio::spawn(echo_connections(listener));
        Ok(addr)
    }

    /// Принимает соединения эхо-сервера, каждое — в своей задаче.
    async fn echo_connections(listener: TcpListener) {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    }

    /// Порт, на котором гарантированно никто не слушает:
    /// bind и немедленный drop слушателя.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("bind ephemeral listener")
            .port()
    }

    /// Минимальный клиент: согласование метода `\x05\x01\x00` → `\x05\x00`.
    async fn negotiate(client: &mut TcpStream) -> io::Result<()> {
        client.write_all(&[0x05, 0x01, 0x00]).await?;
        let mut method = [0u8; 2];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut method))
            .await
            .expect("method selection within timeout")?;
        assert_eq!(&method, &[0x05, 0x00], "server must select no-auth");
        Ok(())
    }

    /// Читает полный 10-байтовый ответ сервера, возвращает REP (второй байт).
    async fn read_reply(client: &mut TcpStream) -> io::Result<u8> {
        let mut reply = [0u8; 10];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut reply))
            .await
            .expect("reply within timeout")?;
        assert_eq!(reply[0], 0x05, "reply VER must be 5");
        assert_eq!(reply[2], 0x00, "reply RSV must be 0");
        Ok(reply[1])
    }

    /// Запрос CONNECT (VER=5, CMD=1) на IPv4-адрес.
    fn connect_ipv4(addr: SocketAddr) -> Vec<u8> {
        let [a, b, c, d] = match addr.ip() {
            std::net::IpAddr::V4(ip) => ip.octets(),
            std::net::IpAddr::V6(_) => panic!("test target must be IPv4"),
        };
        let port = addr.port();
        vec![
            0x05,
            0x01,
            0x00,
            0x01,
            a,
            b,
            c,
            d,
            (port >> 8) as u8,
            port as u8,
        ]
    }

    #[tokio::test]
    async fn ipv4_connect_to_echo_relays_data() -> io::Result<()> {
        let echo_addr = spawn_echo().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        negotiate(&mut client).await?;
        client.write_all(&connect_ipv4(echo_addr)).await?;
        assert_eq!(read_reply(&mut client).await?, 0x00, "CONNECT must succeed");

        client.write_all(b"ping via socks").await?;
        let mut buf = [0u8; 14];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut buf))
            .await
            .expect("echo within timeout")?;
        assert_eq!(&buf, b"ping via socks");

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn refused_target_replies_05_and_connection_survives() -> io::Result<()> {
        let echo_addr = spawn_echo().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        negotiate(&mut client).await?;
        let dead = SocketAddr::from(([127, 0, 0, 1], free_port()));
        client.write_all(&connect_ipv4(dead)).await?;
        assert_eq!(
            read_reply(&mut client).await?,
            0x05,
            "refused dial maps to REP 0x05"
        );

        // Соединение живо после отказа: повторный CONNECT на том же соединении.
        client.write_all(&connect_ipv4(echo_addr)).await?;
        assert_eq!(read_reply(&mut client).await?, 0x00);
        client.write_all(b"retry").await?;
        let mut buf = [0u8; 5];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut buf))
            .await
            .expect("echo within timeout")?;
        assert_eq!(&buf, b"retry");

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn bind_command_rejected_with_07() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        negotiate(&mut client).await?;
        // VER=5, CMD=2 (BIND), RSV=0, ATYP=1, 127.0.0.1:80.
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
            .await?;
        assert_eq!(
            read_reply(&mut client).await?,
            0x07,
            "BIND must map to REP 0x07"
        );

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn domain_connect_to_localhost_echo_relays_data() -> io::Result<()> {
        let echo_addr = spawn_echo().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        negotiate(&mut client).await?;
        let host = b"localhost";
        let port = echo_addr.port();
        let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        request.extend_from_slice(host);
        request.extend_from_slice(&port.to_be_bytes());
        client.write_all(&request).await?;
        assert_eq!(
            read_reply(&mut client).await?,
            0x00,
            "domain CONNECT must succeed"
        );

        client.write_all(b"domain ping").await?;
        let mut buf = [0u8; 11];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut buf))
            .await
            .expect("echo within timeout")?;
        assert_eq!(&buf, b"domain ping");

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_auth_method_gets_ff_and_close() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        // Предлагаем только GSSAPI (0x01): no-auth нет в списке.
        client.write_all(&[0x05, 0x01, 0x01]).await?;
        let mut method = [0u8; 2];
        tokio::time::timeout(TIMEOUT, client.read_exact(&mut method))
            .await
            .expect("method reply within timeout")?;
        assert_eq!(
            &method,
            &[0x05, 0xFF],
            "server must reply NO ACCEPTABLE METHODS"
        );

        // После 0xFF сервер закрывает соединение.
        let mut tail = [0u8; 1];
        let outcome = tokio::time::timeout(TIMEOUT, client.read(&mut tail))
            .await
            .expect("connection closed within timeout");
        match outcome {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Ok(n) => panic!("unexpected data after 0xFF: {n} bytes"),
            Err(e) => panic!("unexpected read error after 0xFF: {e}"),
        }

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn malformed_request_version_closes_connection() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socks_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_socks5(listener));

        let mut client = TcpStream::connect(socks_addr).await?;
        negotiate(&mut client).await?;
        // VER=4 в запросе: некорректный протокол, сервер молча закрывает.
        client
            .write_all(&[0x04, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
            .await?;

        let mut tail = [0u8; 1];
        let outcome = tokio::time::timeout(TIMEOUT, client.read(&mut tail))
            .await
            .expect("connection closed within timeout");
        match outcome {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Ok(n) => panic!("unexpected data after malformed request: {n} bytes"),
            Err(e) => panic!("unexpected read error after malformed request: {e}"),
        }

        assert!(!server.is_finished(), "accept loop must survive");
        server.abort();
        Ok(())
    }
}
