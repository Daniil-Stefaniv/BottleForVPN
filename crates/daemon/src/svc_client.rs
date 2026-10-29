//! Клиент службы маршрутов `pogrebok-svc` на стороне демона (M2 T7).
//!
//! Демон держит одно долгоживущее соединение с службой: каждое
//! pipe-соединение — сессия (маршруты привязаны к ней и снимаются при
//! разрыве, спека §9), поэтому [`SvcTransport`] открывает соединение
//! один раз на всё время жизни, а [`SvcClient::connect`] выполняет
//! рукопожатие `Hello{daemon_pid}` (PID демона — идентификатор для
//! журнала службы). Кадры — [`bottle_core::frame::LengthJsonCodec`]
//! поверх named pipe [`SVC_PIPE`].
//!
//! Ошибки службы возвращаются пользователю её русским сообщением
//! ([`DaemonError::Svc`]); wire-код ошибки (`svc_err`) уходит в лог
//! `warn`, а не в пользовательский текст.

use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bottle_core::svcrpc::{RouteEntry, SvcReply, SvcRequest};

use crate::error::DaemonError;

/// Сообщение для пользователя при отсутствии службы маршрутов
/// (спека §2: UAC-fallback не предусмотрен — установка явная).
pub const SVC_MISSING_MESSAGE: &str = "Служба маршрутов Погребка не установлена — host-бутылки недоступны (pogrebok-svc --install от администратора)";

/// Транспорт к службе маршрутов: одно соединение на время жизни
/// трейта (сессия службы живёт, пока соединение открыто).
///
/// `request` — строгий запрос-ответ: отправляет кадр запроса и
/// возвращает кадр ответа на него.
#[async_trait]
pub trait SvcTransport: Send + Sync {
    /// Выполняет обмен «запрос → ответ» со службой маршрутов.
    async fn request(&self, req: &SvcRequest) -> io::Result<SvcReply>;
}

/// Есть ли канал службы маршрутов (быстрый probe без блокировки).
///
/// Windows: `fs::metadata` на пути канала работает для named pipes и
/// не подключается к ним — проверяется только существование имени.
/// Прочие платформы — всегда `false` (named pipe — транспорт Windows);
/// хост-менеджер (T9) до подключения к службе сверяется с этим probe
/// и при `false` отвечает пользователю [`SVC_MISSING_MESSAGE`].
pub fn svc_pipe_exists() -> bool {
    #[cfg(windows)]
    {
        use bottle_core::svcrpc::SVC_PIPE;
        std::fs::metadata(SVC_PIPE).is_ok()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// `pogrebok routes cleanup` (спека §9): одна сессия со службой и
/// запрос `Cleanup` — снятие сиротских маршрутов после краха демона.
/// Вызывающий проверяет [`svc_pipe_exists`] и формирует предупреждение
/// при отсутствии службы (portable-режим).
#[cfg_attr(not(windows), allow(dead_code))]
pub async fn cleanup_via_service() -> Result<(), DaemonError> {
    #[cfg(windows)]
    {
        let transport = RealSvcPipe::connect().await?;
        let client = SvcClient::connect(Arc::new(transport)).await?;
        client.cleanup().await
    }
    #[cfg(not(windows))]
    {
        Err(DaemonError::Svc(
            "служба маршрутов доступна только на Windows".to_string(),
        ))
    }
}

/// Клиент службы маршрутов: сессия демона поверх открытого соединения.
///
/// Создаётся через [`SvcClient::connect`] (рукопожатие `Hello`);
/// `add`/`del`/`cleanup`/`list` — запросы в рамках открытой сессии.
pub struct SvcClient {
    /// Транспорт-владелец соединения (одно на время жизни клиента).
    transport: Arc<dyn SvcTransport>,
}

impl SvcClient {
    /// Открывает сессию: `Hello{daemon_pid}` через переданный транспорт.
    ///
    /// `Ok` службы — сессия открыта; `Error` — отказ службы (её
    /// русское сообщение уходит пользователю, код — в лог `warn`).
    pub async fn connect(transport: Arc<dyn SvcTransport>) -> Result<Self, DaemonError> {
        let reply = transport
            .request(&SvcRequest::Hello {
                daemon_pid: std::process::id(),
            })
            .await?;
        match reply {
            SvcReply::Ok => {
                tracing::info!(
                    daemon_pid = std::process::id(),
                    "сессия службы маршрутов открыта"
                );
                Ok(Self { transport })
            }
            SvcReply::Error { code, message } => {
                tracing::warn!(code, "служба маршрутов отвергла Hello");
                Err(DaemonError::Svc(message))
            }
            other => Err(DaemonError::Svc(format!(
                "неожиданный ответ службы на Hello: {other:?}"
            ))),
        }
    }

    /// Добавляет точечный маршрут `dest → gateway` через интерфейс
    /// `iface_alias` (адаптер host-бутылки). Идемпотентен на стороне
    /// службы: повторный `add` живого dest — `Ok` без OS-вызова.
    pub async fn add(
        &self,
        dest: IpAddr,
        gateway: IpAddr,
        iface_alias: &str,
    ) -> Result<(), DaemonError> {
        self.expect_ok(
            SvcRequest::Add {
                dest,
                gateway,
                iface_alias: iface_alias.to_string(),
            },
            "add",
        )
        .await
    }

    /// Удаляет точечный маршрут по назначению (идемпотентен на
    /// стороне службы: отсутствующий маршрут — `Ok`).
    pub async fn del(&self, dest: IpAddr) -> Result<(), DaemonError> {
        self.expect_ok(SvcRequest::Del { dest }, "del").await
    }

    /// Снимает все live-маршруты службы (остановка демона, спека §9).
    pub async fn cleanup(&self) -> Result<(), DaemonError> {
        self.expect_ok(SvcRequest::Cleanup, "cleanup").await
    }

    /// Список активных маршрутов службы (снимок live-set).
    pub async fn list(&self) -> Result<Vec<RouteEntry>, DaemonError> {
        match self.transport.request(&SvcRequest::List).await? {
            SvcReply::RouteList { routes } => Ok(routes),
            SvcReply::Error { code, message } => {
                tracing::warn!(code, "ошибка службы маршрутов на list");
                Err(DaemonError::Svc(message))
            }
            other => Err(DaemonError::Svc(format!(
                "неожиданный ответ службы на list: {other:?}"
            ))),
        }
    }

    /// Отправляет запрос, ожидает `Ok`; ошибки службы — [`DaemonError::Svc`].
    async fn expect_ok(&self, req: SvcRequest, op: &str) -> Result<(), DaemonError> {
        match self.transport.request(&req).await? {
            SvcReply::Ok => Ok(()),
            SvcReply::Error { code, message } => {
                tracing::warn!(code, op, "ошибка службы маршрутов");
                Err(DaemonError::Svc(message))
            }
            other => Err(DaemonError::Svc(format!(
                "неожиданный ответ службы на {op}: {other:?}"
            ))),
        }
    }
}

// ----------------------------------------------------------------------
// Обмен с таймаутом и отравлением (общая часть реального транспорта;
// unit-тестируется на любой платформе).
// ----------------------------------------------------------------------

/// Сообщение отравленного таймаутом транспорта: соединение в
/// рассинхроне, нужен новый клиент (соединение и сессия).
const POISONED_MESSAGE: &str = "транспорт отравлен таймаутом — пересоздайте клиент";

/// Выполняет один обмен с таймаутом; таймаут отравляет транспорт.
///
/// Протокол — строгий запрос-ответ по одному соединению: опоздавший
/// ответ таймаутнутого запроса был бы прочитан как ответ СЛЕДУЮЩЕГО
/// (окно рассинхрона). Поэтому первый таймаут поднимает `poisoned` —
/// все дальнейшие вызовы падают сразу, не выходя на провод; вызывающий
/// пересоздаёт клиент.
///
/// Захват соединения происходит внутри `exchange` — ожидание в очереди
/// к занятому соединению не съедает окно таймаута (и не добавляется к
/// чужому).
async fn exchange_with_timeout<F>(
    exchange: F,
    timeout: Duration,
    poisoned: &AtomicBool,
) -> io::Result<SvcReply>
where
    F: std::future::Future<Output = io::Result<SvcReply>>,
{
    if poisoned.load(Ordering::SeqCst) {
        return Err(io::Error::other(POISONED_MESSAGE));
    }
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => {
            poisoned.store(true, Ordering::SeqCst);
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("служба маршрутов не ответила за {timeout:?}"),
            ))
        }
    }
}

// ----------------------------------------------------------------------
// Windows-транспорт: named pipe SVC_PIPE + LengthJsonCodec
// (паттерн appliance::AgentCodec). Поведение — windows-CI/ручная
// проверка (T9); локально проверяется компиляцией msvc-таргета.
// ----------------------------------------------------------------------

#[cfg(windows)]
mod real {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use bottle_core::frame::{decode_frame, encode_frame, FrameError};
    use bottle_core::svcrpc::{SvcReply, SvcRequest, SVC_PIPE};
    use bytes::BytesMut;
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use tokio::sync::Mutex;
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Decoder, Encoder, Framed};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

    use super::{SvcTransport, POISONED_MESSAGE};

    /// Сколько пытаться подключиться к каналу службы: `ERROR_PIPE_BUSY`
    /// (все экземпляры заняты) и `ERROR_FILE_NOT_FOUND` (служба ещё
    /// поднимается) ретраятся до этого срока.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Пауза между попытками подключения.
    const CONNECT_RETRY_STEP: Duration = Duration::from_millis(200);

    /// Таймаут ответа на один запрос: add/del крутят PowerShell
    /// (спека службы — до 30 с на командлет) с запасом.
    const REPLY_TIMEOUT: Duration = Duration::from_secs(35);

    /// Реальный транспорт: named pipe [`SVC_PIPE`], соединение живёт
    /// от `connect` до падения структуры.
    pub struct RealSvcPipe {
        /// Запрос-ответ по одному соединению — обмен сериализуется
        /// мьютексом (строгий request-response протокол).
        conn: Mutex<Framed<NamedPipeClient, SvcCodec>>,
        /// Отравлен таймаутом ([`super::exchange_with_timeout`]):
        /// опоздавший ответ не должен удовлетворить следующий запрос —
        /// транспорт требует пересоздания (новое соединение/сессия).
        poisoned: AtomicBool,
    }

    impl RealSvcPipe {
        /// Подключается к каналу службы с ретраями «занят/не найден»
        /// до [`CONNECT_TIMEOUT`]; прочие ошибки (например, отказ в
        /// доступе) возвращаются сразу.
        pub async fn connect() -> io::Result<Self> {
            let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
            loop {
                match ClientOptions::new().open(SVC_PIPE) {
                    Ok(client) => {
                        tracing::info!(pipe = SVC_PIPE, "соединение со службой маршрутов открыто");
                        return Ok(Self {
                            conn: Mutex::new(Framed::new(client, SvcCodec)),
                            poisoned: AtomicBool::new(false),
                        });
                    }
                    Err(error) => {
                        let retryable = matches!(
                            error.raw_os_error(),
                            Some(code)
                                if code as u32 == ERROR_PIPE_BUSY
                                    || code as u32 == ERROR_FILE_NOT_FOUND
                        );
                        if !retryable
                            || tokio::time::Instant::now() + CONNECT_RETRY_STEP >= deadline
                        {
                            tracing::warn!(%error, "не удалось подключиться к службе маршрутов");
                            return Err(error);
                        }
                        tracing::debug!(%error, "служба маршрутов ещё не готова, повтор подключения");
                        tokio::time::sleep(CONNECT_RETRY_STEP).await;
                    }
                }
            }
        }
    }

    #[async_trait]
    impl SvcTransport for RealSvcPipe {
        async fn request(&self, req: &SvcRequest) -> io::Result<SvcReply> {
            // Кодирование, захват соединения, запись и чтение — всё
            // внутри обмениваемого future: ожидание мьютекса в очереди
            // не добавляется к окну таймаута.
            let exchange = async {
                let mut conn = self.conn.lock().await;
                // Раздражитель из ledger (T9): ожидание в очереди могло
                // пережить таймаут соседнего обмена — транспорт уже
                // отравлен, на провод не выходим.
                if self.poisoned.load(Ordering::SeqCst) {
                    return Err(io::Error::other(POISONED_MESSAGE));
                }
                let mut buf = BytesMut::new();
                conn.codec_mut()
                    .encode(req.clone(), &mut buf)
                    .map_err(io::Error::other)?;
                conn.get_mut().write_all(&buf).await?;
                match conn.next().await {
                    Some(Ok(reply)) => Ok(reply),
                    Some(Err(error)) => Err(io::Error::other(format!(
                        "некорректный кадр от службы маршрутов: {error}"
                    ))),
                    None => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "служба маршрутов закрыла соединение",
                    )),
                }
            };
            super::exchange_with_timeout(exchange, REPLY_TIMEOUT, &self.poisoned).await
        }
    }

    /// Кодек стороны демона: кодирует [`SvcRequest`], декодирует
    /// [`SvcReply`] (кадры — `bottle_core::frame`).
    struct SvcCodec;

    impl Encoder<SvcRequest> for SvcCodec {
        type Error = FrameError;

        fn encode(&mut self, item: SvcRequest, dst: &mut BytesMut) -> Result<(), FrameError> {
            // encode_frame не падает для типов протокола (см. frame.rs).
            dst.extend_from_slice(&encode_frame(&item));
            Ok(())
        }
    }

    impl Decoder for SvcCodec {
        type Item = SvcReply;
        type Error = FrameError;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<SvcReply>, FrameError> {
            decode_frame(src)
        }
    }
}

#[cfg(windows)]
pub use real::RealSvcPipe;

/// Debug без соединения: транспорт не выводится ( dyn-объект).
impl std::fmt::Debug for SvcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SvcClient").finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use bottle_core::frame::LengthJsonCodec;
    use bottle_core::svcrpc::{svc_err, RouteEntry, SvcReply, SvcRequest};
    use bytes::BytesMut;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use tokio_util::codec::{Decoder, Encoder};

    use super::*;
    use crate::error::DaemonError;

    /// Кодирует и декодирует значение через `LengthJsonCodec` — пин
    /// проводного формата: фейк-служба отвечает только тем, что
    /// реально переживает кадр туда-обратно.
    fn round_trip<T>(value: T) -> T
    where
        T: Serialize + Clone + DeserializeOwned,
    {
        let mut codec = LengthJsonCodec::<T>::new();
        let mut buf = BytesMut::new();
        codec
            .encode(value.clone(), &mut buf)
            .expect("encode must succeed for svcrpc types");
        codec
            .decode(&mut buf)
            .expect("decode must not fail on a well-formed frame")
            .expect("buffer must contain a complete frame")
    }

    /// Фейк-служба: скриптованные ответы по порядку, журнал запросов
    /// (после round-trip через кодек), флаг «служба закрыла
    /// соединение». Скрипт обязан покрывать каждый обмен: исчерпание
    /// — паника с именем неожиданного запроса (дефолтного ответа нет —
    /// иначе тест забывший запрограммировать ответ проходит пусто).
    struct FakeSvc {
        requests: StdMutex<Vec<SvcRequest>>,
        replies: StdMutex<VecDeque<SvcReply>>,
        closed: AtomicBool,
    }

    impl FakeSvc {
        fn new(replies: Vec<SvcReply>) -> Arc<Self> {
            Arc::new(Self {
                requests: StdMutex::new(Vec::new()),
                replies: StdMutex::new(replies.into()),
                closed: AtomicBool::new(false),
            })
        }

        /// Служба закрывает соединение (демон должен увидеть io-ошибку).
        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }

        fn requests(&self) -> Vec<SvcRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn next_reply(&self, req: &SvcRequest) -> SvcReply {
            self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                panic!("фейк-служба: нет запрограммированного ответа на запрос {req:?}")
            })
        }
    }

    #[async_trait]
    impl SvcTransport for FakeSvc {
        async fn request(&self, req: &SvcRequest) -> io::Result<SvcReply> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "соединение закрыто службой (фейк)",
                ));
            }
            // Запрос и ответ гоняются через настоящий кодек канала.
            let on_wire = round_trip(req.clone());
            self.requests.lock().unwrap().push(on_wire);
            Ok(round_trip(self.next_reply(req)))
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    fn sample_entry(dest: &str) -> RouteEntry {
        RouteEntry {
            dest: ip(dest),
            gateway: ip("192.168.1.1"),
            iface_alias: "Ethernet".to_string(),
            session: 7,
        }
    }

    #[tokio::test]
    async fn connect_hello_ok_opens_session_with_daemon_pid() {
        // Оба обмена (Hello и add) запрограммированы явно.
        let fake = FakeSvc::new(vec![SvcReply::Ok, SvcReply::Ok]);
        let client = SvcClient::connect(fake.clone()).await.expect("Hello → Ok");

        // Проверяем, что Hello реально ушёл (и пережил кодек) с PID демона.
        assert_eq!(
            fake.requests(),
            vec![SvcRequest::Hello {
                daemon_pid: std::process::id()
            }],
            "клиент начинает обмен с Hello{{daemon_pid}}"
        );
        // После connect можно слать запросы в открытую сессию.
        client
            .add(ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .await
            .expect("add в открытой сессии");
    }

    #[tokio::test]
    async fn connect_hello_error_maps_service_message() {
        let fake = FakeSvc::new(vec![SvcReply::Error {
            code: svc_err::VALIDATION.to_string(),
            message: "сессия не открыта: начните обмен с Hello".to_string(),
        }]);

        let error = SvcClient::connect(fake.clone())
            .await
            .expect_err("служба отвергла Hello");

        assert!(
            matches!(error, DaemonError::Svc(ref message) if message.contains("Hello")),
            "текст службы уходит пользователю: {error}"
        );
        assert_eq!(fake.requests().len(), 1, "ровно один Hello");
    }

    #[tokio::test]
    async fn add_del_cleanup_send_exact_requests() {
        // Hello + add + del + cleanup — по ответу на каждый.
        let fake = FakeSvc::new(vec![SvcReply::Ok, SvcReply::Ok, SvcReply::Ok, SvcReply::Ok]);
        let client = SvcClient::connect(fake.clone()).await.expect("сессия");

        client
            .add(ip("203.0.113.10"), ip("192.168.1.1"), "Wi-Fi")
            .await
            .expect("add → Ok");
        client.del(ip("203.0.113.10")).await.expect("del → Ok");
        client.cleanup().await.expect("cleanup → Ok");

        assert_eq!(
            fake.requests(),
            vec![
                SvcRequest::Hello {
                    daemon_pid: std::process::id()
                },
                SvcRequest::Add {
                    dest: ip("203.0.113.10"),
                    gateway: ip("192.168.1.1"),
                    iface_alias: "Wi-Fi".to_string(),
                },
                SvcRequest::Del {
                    dest: ip("203.0.113.10")
                },
                SvcRequest::Cleanup,
            ]
        );
    }

    #[tokio::test]
    async fn add_validation_error_carries_russian_text() {
        // Первый ответ — Hello (Ok), второй — отказ add.
        let fake = FakeSvc::new(vec![
            SvcReply::Ok,
            SvcReply::Error {
                code: svc_err::VALIDATION.to_string(),
                message: "адрес назначения loopback (127.0.0.0/8, ::1)".to_string(),
            },
        ]);
        let client = SvcClient::connect(fake.clone()).await.expect("сессия");

        let error = client
            .add(ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .await
            .expect_err("служба отвергла add");

        assert!(
            matches!(error, DaemonError::Svc(ref message) if message.contains("loopback")),
            "русский текст ошибки валидации службы: {error}"
        );
    }

    #[tokio::test]
    async fn closed_connection_maps_io_error_to_daemon_error() {
        // Ответ — только на Hello; add ударяется в закрытое соединение.
        let fake = FakeSvc::new(vec![SvcReply::Ok]);
        let client = SvcClient::connect(fake.clone()).await.expect("сессия");

        // Служба умерла/закрыла соединение — следующий запрос падает
        // io-ошибкой транспорта и доезжает до пользователя как
        // DaemonError (не паника, не зависание).
        fake.close();
        let error = client
            .add(ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .await
            .expect_err("соединение закрыто");
        assert!(
            matches!(error, DaemonError::Io(_)),
            "io-ошибка транспорта → DaemonError::Io: {error:?}"
        );
    }

    #[tokio::test]
    async fn list_maps_route_list_reply() {
        let routes = vec![sample_entry("203.0.113.10"), sample_entry("203.0.113.11")];
        // Первый ответ — Hello (Ok), второй — RouteList на запрос List.
        let fake = FakeSvc::new(vec![
            SvcReply::Ok,
            SvcReply::RouteList {
                routes: routes.clone(),
            },
        ]);
        let client = SvcClient::connect(fake.clone()).await.expect("сессия");

        assert_eq!(client.list().await.expect("list → RouteList"), routes);
        assert_eq!(
            fake.requests().last(),
            Some(&SvcRequest::List),
            "list шлёт именно запрос List"
        );
    }

    #[tokio::test]
    async fn unexpected_reply_on_add_is_error() {
        // Служба ответила RouteList на Add — протокол нарушен, это
        // ошибка, а не успех. Первый ответ — Hello (Ok).
        let fake = FakeSvc::new(vec![SvcReply::Ok, SvcReply::RouteList { routes: vec![] }]);
        let client = SvcClient::connect(fake.clone()).await.expect("сессия");

        let error = client
            .add(ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .await
            .expect_err("неожиданный ответ");
        assert!(
            matches!(error, DaemonError::Svc(ref message) if message.contains("add")),
            "ошибка называет операцию: {error}"
        );
    }

    /// Пин сообщения об отсутствии службы (рулинг плана T7): текст
    /// видит пользователь CLI при host-старте без pogrebok-svc.
    #[test]
    fn svc_missing_message_pins_ruling_text() {
        assert_eq!(
            SVC_MISSING_MESSAGE,
            "Служба маршрутов Погребка не установлена — host-бутылки недоступны (pogrebok-svc --install от администратора)"
        );
    }

    /// Не-windows named pipe не существует — probe всегда `false`
    /// (на Windows probe зависит от окружения и не тестируется юнитом).
    #[cfg(not(windows))]
    #[test]
    fn svc_pipe_exists_is_false_off_windows() {
        assert!(!svc_pipe_exists());
    }

    /// Finding 2 (ревью): таймаут обмена отравляет транспорт —
    /// опоздавший ответ не должен удовлетворить следующий запрос.
    /// После таймаута каждый следующий вызов падает сразу, не выходя
    /// на провод (обмен даже не начинает выполняться).
    #[tokio::test]
    async fn timeout_poisons_transport_next_request_fails_without_wire() {
        let poisoned = AtomicBool::new(false);

        // «Служба молчит»: обмен падает таймаутом.
        let error =
            exchange_with_timeout(std::future::pending(), Duration::from_millis(50), &poisoned)
                .await
                .expect_err("таймаут обмена");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(poisoned.load(Ordering::SeqCst), "транспорт отравлен");

        // Следующий запрос: обмен не выполняется вовсе (провод не
        // тронут), сразу отказ с требованием пересоздать клиент.
        let touched = AtomicBool::new(false);
        let exchange = async {
            touched.store(true, Ordering::SeqCst);
            Ok(SvcReply::Ok)
        };
        let error = exchange_with_timeout(exchange, Duration::from_secs(35), &poisoned)
            .await
            .expect_err("отравленный транспорт отказывает сразу");
        assert_eq!(
            error.to_string(),
            "транспорт отравлен таймаутом — пересоздайте клиент",
            "{error}"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "обмен не начинался: провод не тронут"
        );
    }

    /// Здоровый транспорт остаётся здоровым: успешные обмены подряд
    /// не отравляют его.
    #[tokio::test]
    async fn successful_exchanges_keep_transport_healthy() {
        let poisoned = AtomicBool::new(false);
        for _ in 0..2 {
            let reply = exchange_with_timeout(
                async { Ok(SvcReply::Ok) },
                Duration::from_secs(1),
                &poisoned,
            )
            .await
            .expect("успешный обмен");
            assert_eq!(reply, SvcReply::Ok);
        }
        assert!(!poisoned.load(Ordering::SeqCst));
    }
}
