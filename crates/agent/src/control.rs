//! Control-сервер агента: TCP `0.0.0.0:7777`, один клиент за раз.
//!
//! Соединение начинается с handshake `Hello` (версия протокола + токен),
//! после авторизации диспетчеризуются запросы демона. Кадры —
//! [`bottle_core::frame::LengthJsonCodec`].
//!
//! # Инвариант последовательности соединений
//!
//! Соединения обслуживаются СТРОГО ПО ОДНОМУ (`accept → serve → accept`):
//! это не ограничение реализации, а семантика протокола. Пока соединение
//! живо, агент считает его владельцем состояния: `Up`/`Down`/`Status`
//! упорядочены внутри соединения, и демоны-поколения не перебивают
//! запросы друг друга на проводе. Разрыв соединения = конец сессии
//! владельца: следующий клиент подключается к той же таблице бутылок,
//! но со своим `instance`-поколением (протокол v2 отвергает `Down`
//! чужого поколения). Нарушение инварианта (параллельный accept)
//! сломало бы гонку «проигравший рестарт убивает бутылку победителя»,
//! защищённую сейчас порядком на проводе + STALE_INSTANCE (рулинг
//! M3-бэклога: задокументировано, не кодом).

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bottle_core::frame::LengthJsonCodec;
use bottle_core::protocol::{err, AgentReply, AgentRequest, PROTOCOL_VERSION};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::StreamExt;
use tokio_util::codec::{Encoder, FramedRead};
use tokio_util::sync::CancellationToken;

use crate::bottles::Bottles;

/// Адрес control-сервера: слушаем все интерфейсы аплаенса.
pub const CONTROL_BIND_ADDR: &str = "0.0.0.0:7777";

/// Конфигурация control-сервера.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    /// Путь к файлу токена авторизации.
    pub token_path: PathBuf,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            token_path: PathBuf::from("/etc/bottle-agent/token"),
        }
    }
}

/// Состояние агента на время работы control-сервера.
///
/// `authed` сбрасывается при каждом новом соединении: каждый клиент
/// обязан пройти `Hello` заново.
pub struct AgentState {
    /// Ожидаемый токен авторизации.
    pub token: String,
    /// IP аплаенса в сети WSL NAT (сообщается в `HelloAck`).
    pub vm_ip: Ipv4Addr,
    /// Прошёл ли текущий клиент handshake `Hello`.
    pub authed: AtomicBool,
    /// Реестр бутылок: сюда диспетчеризуются `Up`/`Down`/`Status`.
    pub bottles: Arc<Bottles>,
}

/// Debug маскирует токен: derive печатал бы секрет целиком, а логи
/// агента доступны диагностики аплаенса.
impl fmt::Debug for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentState")
            .field("token", &"<скрыт>")
            .field("vm_ip", &self.vm_ip)
            .field("authed", &self.authed)
            .field("bottles", &self.bottles.status(None).len())
            .finish()
    }
}

impl AgentState {
    /// Создаёт состояние с ещё не авторизованным клиентом.
    pub fn new(token: String, vm_ip: Ipv4Addr, bottles: Arc<Bottles>) -> Self {
        Self {
            token,
            vm_ip,
            authed: AtomicBool::new(false),
            bottles,
        }
    }
}

/// Определяет IP интерфейса маршрута по умолчанию (eth0 в аплаенсе).
///
/// UDP-`connect` не отправляет пакеты: ядро лишь выбирает маршрут
/// и локальный адрес, поэтому способ работает без внешней сети.
pub fn detect_vm_ip() -> io::Result<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect("8.8.8.8:80")?;
    match sock.local_addr()? {
        SocketAddr::V4(addr) => Ok(*addr.ip()),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "маршрут по умолчанию не IPv4",
        )),
    }
}

/// Диспетчеризация одного запроса поверх состояния агента.
///
/// Побочные эффекты: `Hello` авторизует клиента, `Up`/`Down`/`Status`
/// уходят в [`Bottles`]. Версия проверяется раньше токена: несовпадение
/// версии диагностируется однозначно.
pub async fn handle_request(state: &AgentState, req: AgentRequest) -> AgentReply {
    match req {
        AgentRequest::Hello { version, token } => {
            if version != PROTOCOL_VERSION {
                return error_reply(
                    err::VERSION,
                    &format!("версия протокола {version} не совпадает с {PROTOCOL_VERSION}"),
                );
            }
            if token != state.token {
                return error_reply(err::AUTH, "неверный токен авторизации");
            }
            state.authed.store(true, Ordering::SeqCst);
            AgentReply::HelloAck {
                version: PROTOCOL_VERSION,
                appliance_version: env!("CARGO_PKG_VERSION").to_string(),
                vm_ip: state.vm_ip,
            }
        }
        // До успешного Hello любые запросы отвергаются.
        _ if !state.authed.load(Ordering::SeqCst) => {
            error_reply(err::AUTH, "сначала Hello с корректным токеном")
        }
        AgentRequest::Up(args) => state.bottles.up(args).await,
        AgentRequest::Down { id, instance } => state.bottles.down(&id, instance).await,
        AgentRequest::Status { id } => AgentReply::Status {
            bottles: state.bottles.status(id.as_deref()),
        },
        AgentRequest::Shutdown => AgentReply::Ok,
    }
}

/// Собирает ответ-ошибку.
fn error_reply(code: &str, message: &str) -> AgentReply {
    AgentReply::Error {
        code: code.to_string(),
        message: message.to_string(),
    }
}

/// Причина завершения соединения с клиентом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnEnd {
    /// Клиент отключился или прислал некорректный кадр.
    Disconnected,
    /// Клиент запросил `Shutdown` и получил `Ok`.
    Shutdown,
}

/// Принимает соединения по одному (план §4.3: один control-клиент),
/// пока не получен `Shutdown` или не отменён `shutdown` извне.
pub async fn serve(state: Arc<AgentState>, shutdown: CancellationToken) -> io::Result<()> {
    let listener = TcpListener::bind(CONTROL_BIND_ADDR).await?;
    tracing::info!(addr = CONTROL_BIND_ADDR, "control-сервер запущен");
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = shutdown.cancelled() => {
                tracing::info!("control-сервер остановлен по отмене");
                return Ok(());
            }
        };
        tracing::info!(%peer, "клиент подключился");
        // Новое соединение обязано пройти Hello заново.
        state.authed.store(false, Ordering::SeqCst);

        match handle_connection(stream, &state, &shutdown, peer).await {
            Ok(ConnEnd::Shutdown) => {
                tracing::info!(%peer, "получен Shutdown, останавливаюсь");
                shutdown.cancel();
                return Ok(());
            }
            Ok(ConnEnd::Disconnected) => tracing::info!(%peer, "клиент отключился"),
            Err(e) => tracing::warn!(%peer, error = %e, "ошибка соединения"),
        }
    }
}

/// Обрабатывает одно соединение до отключения клиента или `Shutdown`.
async fn handle_connection(
    stream: TcpStream,
    state: &AgentState,
    shutdown: &CancellationToken,
    peer: SocketAddr,
) -> io::Result<ConnEnd> {
    stream.set_nodelay(true)?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<AgentRequest>::new());
    let mut reply_codec = LengthJsonCodec::<AgentReply>::new();

    loop {
        let req = tokio::select! {
            maybe = reader.next() => match maybe {
                None => return Ok(ConnEnd::Disconnected),
                Some(Ok(req)) => req,
                Some(Err(e)) => {
                    tracing::warn!(%peer, error = %e, "некорректный кадр, соединение закрывается");
                    return Ok(ConnEnd::Disconnected);
                }
            },
            () = shutdown.cancelled() => return Ok(ConnEnd::Disconnected),
        };

        let is_shutdown = matches!(req, AgentRequest::Shutdown);
        let reply = handle_request(state, req).await;
        if let Err(e) = send_reply(&mut write_half, &mut reply_codec, reply).await {
            tracing::warn!(%peer, error = %e, "не удалось отправить ответ");
            return Ok(if is_shutdown {
                ConnEnd::Shutdown
            } else {
                ConnEnd::Disconnected
            });
        }
        if is_shutdown {
            return Ok(ConnEnd::Shutdown);
        }
    }
}

/// Кодирует и отправляет ответ клиенту.
///
/// Используется fallible-путь `Encoder` (а не паникующий `encode_frame`).
async fn send_reply<W>(
    write_half: &mut W,
    codec: &mut LengthJsonCodec<AgentReply>,
    reply: AgentReply,
) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = bytes::BytesMut::new();
    codec.encode(reply, &mut buf).map_err(io::Error::other)?;
    write_half.write_all(&buf).await
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::bottles::Bottles;
    use bottle_core::profile::TunnelProtocol;
    use bottle_core::protocol::UpArgs;

    fn state() -> AgentState {
        AgentState::new(
            "secret-token".to_string(),
            Ipv4Addr::new(192, 168, 77, 5),
            Arc::new(Bottles::new()),
        )
    }

    fn hello(version: u32, token: &str) -> AgentRequest {
        AgentRequest::Hello {
            version,
            token: token.to_string(),
        }
    }

    async fn authorized(st: &AgentState) {
        handle_request(st, hello(PROTOCOL_VERSION, "secret-token")).await;
    }

    fn error_code(reply: AgentReply) -> String {
        match reply {
            AgentReply::Error { code, .. } => code,
            other => panic!("ожидалась ошибка, получено {other:?}"),
        }
    }

    fn up_args(id: &str) -> UpArgs {
        UpArgs {
            id: id.to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
            dns: None,
            socks5_port: 0,
            forwards: vec![],
            instance: 0,
            username: None,
            password: None,
        }
    }

    #[tokio::test]
    async fn hello_with_valid_token_acks_with_state_vm_ip() {
        let st = state();
        let reply = handle_request(&st, hello(PROTOCOL_VERSION, "secret-token")).await;
        assert_eq!(
            reply,
            AgentReply::HelloAck {
                version: PROTOCOL_VERSION,
                appliance_version: env!("CARGO_PKG_VERSION").to_string(),
                vm_ip: Ipv4Addr::new(192, 168, 77, 5),
            }
        );
        assert!(
            st.authed.load(Ordering::SeqCst),
            "успешный Hello авторизует"
        );
    }

    #[tokio::test]
    async fn hello_with_wrong_token_is_auth_error() {
        let reply = handle_request(&state(), hello(PROTOCOL_VERSION, "wrong-token")).await;
        assert_eq!(error_code(reply), err::AUTH);
    }

    #[tokio::test]
    async fn hello_with_wrong_version_is_version_error() {
        let reply = handle_request(&state(), hello(99, "secret-token")).await;
        assert_eq!(error_code(reply), err::VERSION);
    }

    #[tokio::test]
    async fn status_before_hello_is_auth_error() {
        let reply = handle_request(&state(), AgentRequest::Status { id: None }).await;
        assert_eq!(error_code(reply), err::AUTH);
    }

    #[tokio::test]
    async fn status_after_hello_returns_empty_bottles() {
        let st = state();
        authorized(&st).await;
        let reply = handle_request(&st, AgentRequest::Status { id: None }).await;
        assert_eq!(reply, AgentReply::Status { bottles: vec![] });
    }

    #[test]
    fn agent_state_debug_masks_token() {
        let debug = format!("{:?}", state());
        assert!(
            !debug.contains("secret-token"),
            "Debug не должен раскрывать токен: {debug}"
        );
        assert!(
            debug.contains("token"),
            "поле token присутствует замаскированным: {debug}"
        );
    }

    #[tokio::test]
    async fn up_with_invalid_id_after_auth_is_bad_request() {
        // Барьер id срабатывает до системных вызовов — тест работает
        // на любой ОС и ничего не создаёт (Review Focus #1).
        let st = state();
        authorized(&st).await;
        for id in ["../evil", "офис"] {
            let reply = handle_request(&st, AgentRequest::Up(up_args(id))).await;
            assert_eq!(error_code(reply), err::BAD_REQUEST, "id {id}");
        }
    }

    #[tokio::test]
    async fn down_unknown_id_after_auth_is_not_found() {
        let st = state();
        authorized(&st).await;
        let reply = handle_request(
            &st,
            AgentRequest::Down {
                id: "ghost".to_string(),
                instance: 0,
            },
        )
        .await;
        assert_eq!(error_code(reply), err::NOT_FOUND);
    }

    #[tokio::test]
    async fn down_with_foreign_instance_is_stale_and_bottle_stays_alive() {
        let st = state();
        authorized(&st).await;
        st.bottles.insert_for_test("office-a", 7);

        let reply = handle_request(
            &st,
            AgentRequest::Down {
                id: "office-a".to_string(),
                instance: 8,
            },
        )
        .await;
        assert_eq!(
            error_code(reply),
            err::STALE_INSTANCE,
            "чужое поколение не опускает бутылку"
        );
        assert_eq!(
            st.bottles.status(Some("office-a")).len(),
            1,
            "бутылка жива после чужого Down"
        );
    }

    #[tokio::test]
    async fn shutdown_after_auth_is_ok() {
        let st = state();
        authorized(&st).await;
        let reply = handle_request(&st, AgentRequest::Shutdown).await;
        assert_eq!(reply, AgentReply::Ok);
    }

    #[tokio::test]
    async fn wrong_hello_does_not_authorize_next_request() {
        let st = state();
        handle_request(&st, hello(PROTOCOL_VERSION, "wrong")).await;
        let reply = handle_request(&st, AgentRequest::Status { id: None }).await;
        assert_eq!(error_code(reply), err::AUTH);
    }
}
