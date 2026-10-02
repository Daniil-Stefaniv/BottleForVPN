//! TCP-проброс: адрес цели, его разрешение и сервер проброса.

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use crate::copy::copy_bidirectional;

/// Адрес цели проброса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    /// Готовый IP-адрес с портом — DNS-разрешение не требуется.
    Ip(SocketAddr),
    /// Доменное имя с портом; разрешается при каждом подключении,
    /// чтобы подхватывать изменения DNS.
    Domain {
        /// Имя хоста (без порта).
        host: String,
        /// Порт цели.
        port: u16,
    },
}

/// Ошибка разбора строки в [`TargetAddr`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid target address {src:?}: {reason}")]
pub struct ParseTargetAddrError {
    /// Исходная строка.
    pub src: String,
    /// Причина отказа.
    pub reason: &'static str,
}

impl FromStr for TargetAddr {
    type Err = ParseTargetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason: &'static str| ParseTargetAddrError {
            src: s.to_owned(),
            reason,
        };
        // Порт допускается только в диапазоне 1..=65535.
        let port_of = |port_str: &str| -> Result<u16, ParseTargetAddrError> {
            let port: u16 = port_str
                .parse()
                .map_err(|_| err("port must be a number in 1..=65535"))?;
            if port == 0 {
                return Err(err("port must be a number in 1..=65535"));
            }
            Ok(port)
        };

        // IPv6-литерал в скобках: `[::1]:3389`.
        if let Some(rest) = s.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| err("unterminated bracketed IPv6 literal"))?;
            let port = port_of(
                tail.strip_prefix(':')
                    .ok_or_else(|| err("missing port after bracketed IPv6 literal"))?,
            )?;
            let ip: Ipv6Addr = host
                .parse()
                .map_err(|_| err("invalid bracketed IPv6 literal"))?;
            return Ok(TargetAddr::Ip(SocketAddr::new(IpAddr::V6(ip), port)));
        }

        // Обычная форма `host:port`; режем по последнему `:`,
        // чтобы IPv6-подобные хосты не ломали разбор порта.
        let (host, port_str) = s.rsplit_once(':').ok_or_else(|| err("missing port"))?;
        if host.is_empty() {
            return Err(err("empty host"));
        }
        let port = port_of(port_str)?;

        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(TargetAddr::Ip(SocketAddr::new(ip, port)));
        }
        Ok(TargetAddr::Domain {
            host: host.to_owned(),
            port,
        })
    }
}

/// Разрешает адрес цели в готовый [`SocketAddr`].
///
/// [`TargetAddr::Ip`] возвращается как есть; [`TargetAddr::Domain`]
/// разрешается через `tokio::net::lookup_host`, берётся первый адрес.
pub async fn resolve_target(target: &TargetAddr) -> io::Result<SocketAddr> {
    match target {
        TargetAddr::Ip(addr) => Ok(*addr),
        TargetAddr::Domain { host, port } => {
            let mut addrs = tokio::net::lookup_host((host.as_str(), *port)).await?;
            addrs.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("host {host:?} resolved to no addresses"),
                )
            })
        }
    }
}

/// Future одного установки исходящего соединения (см. [`Dial`]).
pub type DialFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<TcpStream>> + Send>>;

/// Функция установки исходящего соединения с целью — инъекция способа
/// диала (M2 T9): host-релеи демона привязывают сокет к VPN-адаптеру
/// (`IP_UNICAST_IF`/`IPV6_UNICAST_IF`) ДО connect; обычные релеи
/// используют [`default_dial`] (просто `TcpStream::connect`).
pub type Dial = Arc<dyn Fn(SocketAddr) -> DialFuture + Send + Sync>;

/// Стандартный диал: `TcpStream::connect` без дополнительной настройки.
pub fn default_dial() -> Dial {
    Arc::new(|addr: SocketAddr| Box::pin(async move { TcpStream::connect(addr).await }))
}

/// Сервер TCP-проброса: принимает соединения на `listener` и копирует
/// байты в обе стороны с целью `target`.
///
/// Соединения обслуживаются конкурентно (`tokio::spawn`). Цель разрешается
/// заново на каждое соединение, чтобы подхватывать изменения DNS; на обоих
/// сокетах включается `TCP_NODELAY`. Отказ отдельного соединения (DNS,
/// недоступность цели, обрыв копирования) закрывает только его и логируется
/// через `tracing`, не останавливая сервер. Ошибка возвращается только при
/// отказе самого слушателя (`accept`) — дальше сервер работать не может.
pub async fn serve_forward(listener: TcpListener, target: TargetAddr) -> io::Result<()> {
    serve_forward_with_dial(listener, target, default_dial()).await
}

/// [`serve_forward`] с инъекцией способа установки исходящего соединения
/// ([`Dial`]): host-релеи демона передают диал с привязкой сокета
/// к интерфейсу VPN (спека §7.2, шаг 4).
pub async fn serve_forward_with_dial(
    listener: TcpListener,
    target: TargetAddr,
    dial: Dial,
) -> io::Result<()> {
    loop {
        let (client, peer) = listener.accept().await?;
        let connection_target = target.clone();
        let dial = Arc::clone(&dial);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(client, peer, &connection_target, &dial).await {
                tracing::warn!(%peer, %error, "forward connection failed");
            }
        });
    }
}

/// Обслуживает одно соединение: разрешает цель, подключается (через
/// переданный [`Dial`]), копирует байты.
async fn handle_connection(
    client: TcpStream,
    peer: SocketAddr,
    target: &TargetAddr,
    dial: &Dial,
) -> io::Result<()> {
    client.set_nodelay(true)?;
    let addr = resolve_target(target).await?;
    let upstream = dial(addr).await?;
    upstream.set_nodelay(true)?;
    tracing::debug!(%peer, target = %addr, "forward connection established");

    copy_bidirectional(client, upstream).await?;
    tracing::debug!(%peer, target = %addr, "forward connection closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Эхо-сервер: слушает `127.0.0.1:0`, возвращает те же байты.
    /// Порождённые задачи живут в тестовом рантайме и закрываются
    /// вместе с ним (shutdown on drop).
    async fn spawn_echo() -> io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
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
        });
        Ok(addr)
    }

    /// Свободный порт: bind и немедленный drop слушателя.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("bind ephemeral listener")
            .port()
    }

    #[tokio::test]
    async fn echo_through_forward() -> io::Result<()> {
        let echo_addr = spawn_echo().await?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let forward_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_forward(listener, TargetAddr::Ip(echo_addr)));

        let mut client = TcpStream::connect(forward_addr).await?;
        client.write_all(b"ping").await?;

        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut buf))
            .await
            .expect("echo within timeout")?;
        assert_eq!(&buf, b"ping");

        // Сервер продолжает работать после обслуженного соединения.
        assert!(!server.is_finished());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn dead_target_closes_only_that_connection() -> io::Result<()> {
        // Порт, на котором никто не слушает: подключение к цели упадёт.
        let dead_target = TargetAddr::Ip(SocketAddr::from(([127, 0, 0, 1], free_port())));

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let forward_addr = listener.local_addr()?;
        let server = tokio::spawn(serve_forward(listener, dead_target));

        let mut first = TcpStream::connect(forward_addr).await?;
        first.write_all(b"ping").await?;
        // Провал цели обрывает соединение: непрочитанные прокси байты
        // превращают FIN в RST, поэтому равнозначны Ok(0) и ConnectionReset.
        let mut buf = [0u8; 16];
        let outcome = tokio::time::timeout(Duration::from_secs(5), first.read(&mut buf))
            .await
            .expect("connection closed within timeout");
        match outcome {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Ok(n) => panic!("unexpected data from dead target: {n} bytes"),
            Err(e) => panic!("unexpected read error from dead target: {e}"),
        }
        drop(first);

        // Сервер пережил отказ цели и принимает следующие соединения.
        assert!(!server.is_finished());
        let second = TcpStream::connect(forward_addr).await?;
        assert!(
            !server.is_finished(),
            "accept loop must survive connection failures"
        );
        drop(second);
        server.abort();
        Ok(())
    }

    #[test]
    fn parses_ip_v4_with_port() {
        let addr: TargetAddr = "10.20.30.15:3389".parse().expect("valid IPv4 target");
        assert_eq!(
            addr,
            TargetAddr::Ip(SocketAddr::from(([10, 20, 30, 15], 3389)))
        );
    }

    #[test]
    fn parses_bracketed_ip_v6_with_port() {
        let addr: TargetAddr = "[::1]:3389".parse().expect("valid IPv6 target");
        assert_eq!(
            addr,
            TargetAddr::Ip(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 3389)))
        );
    }

    #[test]
    fn parses_domain_with_port() {
        let addr: TargetAddr = "host.example:443".parse().expect("valid domain target");
        assert_eq!(
            addr,
            TargetAddr::Domain {
                host: "host.example".to_string(),
                port: 443
            }
        );
    }

    #[test]
    fn rejects_invalid_targets() {
        for src in [
            "nohost",
            "",
            ":80",
            "host:",
            "host:0",
            "host:65536",
            "host:notaport",
            "[::1]",
            "[::1]:0",
            "[oops]:80",
            "[]:80",
        ] {
            let res: Result<TargetAddr, _> = src.parse();
            assert!(res.is_err(), "must be rejected: {src:?}");
        }
    }

    #[tokio::test]
    async fn resolves_ip_target_as_is() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        assert_eq!(resolve_target(&TargetAddr::Ip(addr)).await.unwrap(), addr);
    }

    #[tokio::test]
    async fn resolves_domain_via_dns() {
        // localhost не требует внешнего DNS и всегда резолвится в loopback.
        let resolved = resolve_target(&TargetAddr::Domain {
            host: "localhost".to_string(),
            port: 80,
        })
        .await
        .expect("resolve localhost");
        assert!(resolved.ip().is_loopback());
    }
}
