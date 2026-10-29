//! Клиент демона в GUI (0.3, спека §5): control-соединение запрос/ответ
//! и живое event-соединение (Subscribe) с переподключением.
//!
//! Транспорт — тот же, что у демона: UDS `<data>/daemon.sock` на
//! macOS/Unix, named pipe `\\.\pipe\pogrebok-daemon` на Windows; кадры
//! `serde_json::Value` через `LengthJsonCodec`. Авторизация — токеном,
//! который GUI сгенерировал и передал демону аргументом `--gui-token`
//! при спавне (живёт только в памяти GUI).
//!
//! Разрыв event-соединения не роняет GUI: цикл эмитит
//! `daemon-disconnected` и переподключается с backoff
//! 0.5/1/2/5 с (максимум [`MAX_RECONNECTS`] попыток, Review Focus #3).

// Публичный API проводится в Tauri-команды в задачах 14–17 (экран
// блокировки, главное окно, Диагностика); до них используют только тесты.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bottle_core::frame::LengthJsonCodec;
use bottle_core::ipc::{DaemonEvent, FullProfile, GuiReply, GuiRequest, AUTH_ERR};
#[cfg(windows)]
use bottle_core::ipc::DAEMON_PIPE;
use bytes::BytesMut;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tokio_util::codec::{Encoder, FramedRead};

/// Таймаут ответа демона на запрос control-соединения.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Окно ожидания появления демона при подключении (демон может ещё
/// подниматься после спавна).
const CONNECT_WINDOW: Duration = Duration::from_secs(3);

/// Максимум попыток переподключения event-цикла подряд.
const MAX_RECONNECTS: u32 = 10;

/// Событие фронтенда: разрыв event-соединения (перед переподключением).
pub const EVT_DISCONNECTED: &str = "daemon-disconnected";

/// Событие фронтенда: кадр DaemonEvent от демона.
pub const EVT_EVENT: &str = "daemon-event";

/// Эндпоинт демона: UDS-путь (unix) или имя named pipe (windows).
#[derive(Debug, Clone)]
enum Endpoint {
    #[cfg(unix)]
    Socket(PathBuf),
    #[cfg(windows)]
    Pipe(String),
}

/// Тип-объект транспорта: UnixStream (unix) или NamedPipeClient (windows).
trait Pipe: AsyncRead + AsyncWrite + Unpin + Send {}

#[cfg(unix)]
impl Pipe for tokio::net::UnixStream {}

#[cfg(windows)]
impl Pipe for tokio::net::windows::named_pipe::NamedPipeClient {}

/// Полудуплексы control-соединения (кадры Value — нейтральный формат
/// сессии, см. daemon::session).
struct ControlConn {
    reader: FramedRead<ReadHalf<Box<dyn Pipe>>, LengthJsonCodec<Value>>,
    writer: WriteHalf<Box<dyn Pipe>>,
}

struct Inner {
    endpoint: Endpoint,
    control: Mutex<Option<ControlConn>>,
    connect_window: Duration,
}

/// Клиент демона: одно control-соединение (запрос/ответ под мьютексом)
/// и отдельное event-соединение в фоновой задаче. Клонируемый (Arc).
#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<Inner>,
}

/// Коллбэк доставки событий во фронтенд: (имя события, полезная нагрузка).
/// Отделён от Tauri, чтобы цикл подписки тестировался без AppHandle.
type Emitter = Arc<dyn Fn(&str, Value) + Send + Sync>;

impl DaemonClient {
    /// Клиент штатного эндпоинта демона (`<data>/daemon.sock` /
    /// `\\.\pipe\pogrebok-daemon`); `None` — каталог данных недоступен.
    pub fn new() -> Option<Self> {
        #[cfg(unix)]
        {
            let data = bottle_core::ipc::default_data_dir()?;
            Some(Self::with_endpoint(Endpoint::Socket(
                bottle_core::ipc::daemon_socket_path(&data),
            )))
        }
        #[cfg(windows)]
        {
            Some(Self::with_endpoint(Endpoint::Pipe(DAEMON_PIPE.to_string())))
        }
    }

    fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            inner: Arc::new(Inner {
                endpoint,
                control: Mutex::new(None),
                connect_window: CONNECT_WINDOW,
            }),
        }
    }

    /// Тестовый шов (unix): клиент произвольного сокета и короткого окна.
    #[cfg(all(test, unix))]
    fn with_socket(path: PathBuf) -> Self {
        let mut client = Self::with_endpoint(Endpoint::Socket(path));
        Arc::get_mut(&mut client.inner)
            .expect("свежий клиент без клонов")
            .connect_window = Duration::from_millis(500);
        client
    }

    /// Подключает control-соединение: handshake `Hello { token }`.
    /// Неверный токен — ошибка `AUTH: …` (демон закрывает соединение).
    pub async fn connect(&self, token: &str) -> Result<(), String> {
        let pipe = open(&self.inner.endpoint, self.inner.connect_window).await?;
        let (read_half, write_half) = tokio::io::split(pipe);
        let mut conn = ControlConn {
            reader: FramedRead::new(read_half, LengthJsonCodec::<Value>::new()),
            writer: write_half,
        };
        send_frame(&mut conn.writer, &GuiRequest::Hello { token: token.into() }).await?;
        match read_reply(&mut conn.reader).await? {
            GuiReply::Ok => {
                *self.inner.control.lock().await = Some(conn);
                Ok(())
            }
            GuiReply::Error { message } => Err(message),
            other => Err(format!("неожиданный ответ демона на Hello: {other:?}")),
        }
    }

    /// Запрос к демону по control-соединению (после [`Self::connect`]).
    /// Транспортная ошибка рвёт соединение: следующий запрос получит
    /// «нет соединения», пока GUI не переподключится.
    pub async fn request(&self, req: GuiRequest) -> Result<GuiReply, String> {
        let mut guard = self.inner.control.lock().await;
        let conn = guard
            .as_mut()
            .ok_or_else(|| "нет соединения с демоном — запустите демона".to_string())?;
        let result = async {
            send_frame(&mut conn.writer, &req).await?;
            read_reply(&mut conn.reader).await
        }
        .await;
        if result.is_err() {
            *guard = None; // разрыв: не переиспользуем подозрительный канал
        }
        result
    }

    /// Полная синхронизация реестра профилей (после разблокировки БД и
    /// каждой мутации — через Upsert/Remove; sync_all — при старте).
    pub async fn sync_all(&self, profiles: Vec<FullProfile>) -> Result<(), String> {
        match self.request(GuiRequest::SyncProfiles { profiles }).await? {
            GuiReply::Ok => Ok(()),
            GuiReply::Error { message } => Err(message),
            other => Err(format!("неожиданный ответ демона на SyncProfiles: {other:?}")),
        }
    }

    /// Запускает фоновой цикл подписки: Subscribe → поток событий в
    /// фронтенд; разрыв → `daemon-disconnected` + backoff-реконнект.
    pub fn spawn_event_loop(&self, app: tauri::AppHandle, token: String) {
        let inner = Arc::clone(&self.inner);
        let emit: Emitter = Arc::new(move |name, payload| {
            use tauri::Emitter as _;
            let _ = app.emit(name, payload);
        });
        tokio::spawn(run_event_loop(inner, token, emit, default_backoff, MAX_RECONNECTS));
    }
}

/// Цикл подписки: пока живо — читает DaemonEvent и эмитит их; при
/// разрыве эмитит `daemon-disconnected` и переподключается с backoff.
/// Ошибка AUTH — фатальна (токен сам себя не починит), цикл завершается.
async fn run_event_loop(
    inner: Arc<Inner>,
    token: String,
    emit: Emitter,
    backoff: fn(u32) -> Duration,
    max_attempts: u32,
) {
    let mut attempt = 0u32;
    loop {
        match subscribe_once(&inner.endpoint, &token, &emit, inner.connect_window).await {
            Ok(()) | Err(SubError::Transport(_)) => {}
            Err(SubError::Auth(message)) => {
                tracing::error!(%message, "подписка отклонена демоном — цикл остановлен");
                return;
            }
        }
        attempt += 1;
        if attempt > max_attempts {
            return;
        }
        emit(EVT_DISCONNECTED, Value::Null);
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// Backoff переподключения: 0.5 / 1 / 2 / 5 с, дальше держим 5 с.
fn default_backoff(attempt: u32) -> Duration {
    const STEPS: [Duration; 4] = [
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(5),
    ];
    STEPS[(attempt as usize).min(STEPS.len() - 1)]
}

/// Ошибка одной подписки: транспортная (переподключаемся) или AUTH
/// (фатально).
enum SubError {
    Transport(String),
    Auth(String),
}

/// Одна жизнь event-соединения: подключение → Subscribe → поток кадров
/// DaemonEvent до разрыва (Ok при чистом EOF, Err при ошибке).
async fn subscribe_once(
    endpoint: &Endpoint,
    token: &str,
    emit: &Emitter,
    connect_window: Duration,
) -> Result<(), SubError> {
    let pipe = open(endpoint, connect_window)
        .await
        .map_err(SubError::Transport)?;
    let (read_half, mut write_half) = tokio::io::split(pipe);
    send_frame(&mut write_half, &GuiRequest::Subscribe { token: token.into() })
        .await
        .map_err(SubError::Transport)?;
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<Value>::new());

    // Первый кадр — ответ на Subscribe (Ok / AUTH-ошибка).
    match read_reply(&mut reader).await.map_err(SubError::Transport)? {
        GuiReply::Ok => {}
        GuiReply::Error { message } if message.starts_with(AUTH_ERR) => {
            return Err(SubError::Auth(message));
        }
        other => {
            return Err(SubError::Transport(format!(
                "неожиданный ответ демона на Subscribe: {other:?}"
            )));
        }
    }

    loop {
        match reader.next().await {
            Some(Ok(value)) => match serde_json::from_value::<DaemonEvent>(value.clone()) {
                Ok(_) => emit(EVT_EVENT, value),
                Err(error) => {
                    return Err(SubError::Transport(format!(
                        "битый кадр события демона: {error}"
                    )));
                }
            },
            Some(Err(error)) => {
                return Err(SubError::Transport(format!("ошибка чтения событий: {error}")));
            }
            None => return Ok(()), // демон закрыл соединение — реконнект
        }
    }
}

/// Подключается к демону с retry-окном (демон может ещё подниматься).
async fn open(endpoint: &Endpoint, window: Duration) -> Result<Box<dyn Pipe>, String> {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let attempt: std::io::Result<Box<dyn Pipe>> = match endpoint {
            #[cfg(unix)]
            Endpoint::Socket(path) => tokio::net::UnixStream::connect(path)
                .await
                .map(|stream| Box::new(stream) as Box<dyn Pipe>),
            #[cfg(windows)]
            Endpoint::Pipe(name) => tokio::net::windows::named_pipe::ClientOptions::new()
                .open(name)
                .map(|pipe| Box::new(pipe) as Box<dyn Pipe>),
        };
        match attempt {
            Ok(pipe) => return Ok(pipe),
            Err(error) => {
                let stale = matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                );
                if !stale || tokio::time::Instant::now() >= deadline {
                    return Err(
                        "не удалось подключиться к демону — запущен ли он?".to_string()
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Кодирует значение в нейтральный кадр Value и пишет в соединение.
async fn send_frame<W, T>(writer: &mut W, req: &T) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let value = serde_json::to_value(req)
        .map_err(|error| format!("ошибка кодирования запроса: {error}"))?;
    let mut buf = BytesMut::new();
    LengthJsonCodec::<Value>::new()
        .encode(value, &mut buf)
        .map_err(|error| format!("ошибка обмена с демоном: {error}"))?;
    writer
        .write_all(&buf)
        .await
        .map_err(|error| format!("ошибка обмена с демоном: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("ошибка обмена с демоном: {error}"))
}

/// Читает один кадр ответа с таймаутом и разбирает как GuiReply.
async fn read_reply(
    reader: &mut FramedRead<ReadHalf<Box<dyn Pipe>>, LengthJsonCodec<Value>>,
) -> Result<GuiReply, String> {
    match tokio::time::timeout(REPLY_TIMEOUT, reader.next()).await {
        Ok(Some(Ok(value))) => serde_json::from_value(value)
            .map_err(|error| format!("битый ответ демона: {error}")),
        Ok(Some(Err(error))) => Err(format!("ошибка обмена с демоном: {error}")),
        Ok(None) => Err("демон закрыл соединение без ответа".to_string()),
        Err(_) => Err(format!("демон не ответил за {} с", REPLY_TIMEOUT.as_secs())),
    }
}

/// Генерирует токен GUI: 32 случайных байта → 64 hex-символа. Токен
/// живёт только в памяти GUI и передаётся демону аргументом `--gui-token`.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("ОС обязана дать случайность для токена");
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use bottle_core::ipc::{DaemonStats, VpnCredentials};
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    use super::*;

    const TOKEN: &str = "test-token";

    fn sample_stats() -> DaemonStats {
        DaemonStats {
            pid: 4217,
            version: "0.3.0".to_string(),
            started_at: "2026-10-22T00:00:00.000Z".to_string(),
            active_bottles: 1,
            total_profiles: 2,
            mem_bytes: 64 * 1024 * 1024,
            cpu_pct: 0.1,
            wsl_state: "running".to_string(),
            appliance_version: None,
        }
    }

    fn sample_event(id: &str) -> DaemonEvent {
        DaemonEvent::StateChanged {
            id: id.to_string(),
            state: "up".to_string(),
            detail: None,
            socks5: Some(1080),
            forwards: vec![],
        }
    }

    /// Фейк-сервер демона на UDS: auth-гейт как у настоящей сессии,
    /// Stats/SyncProfiles — заглушки, Subscribe — по сценарию событий.
    struct FakeServer {
        socket: PathBuf,
        requests: mpsc::UnboundedReceiver<Value>,
        _dir: tempfile::TempDir,
        _task: JoinHandle<()>,
    }

    fn spawn_fake(sub_script: Vec<Vec<DaemonEvent>>) -> FakeServer {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let sub_counter = Arc::new(AtomicU32::new(0));

        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let req_tx = req_tx.clone();
                let script = sub_script.clone();
                let counter = Arc::clone(&sub_counter);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader =
                        FramedRead::new(read_half, LengthJsonCodec::<Value>::new());
                    while let Some(frame) = reader.next().await {
                        let Ok(value) = frame else { return };
                        let Ok(req) = serde_json::from_value::<GuiRequest>(value.clone())
                        else {
                            return;
                        };
                        // Подписка: проверка токена, Ok, сценарий событий.
                        if let GuiRequest::Subscribe { token } = &req {
                            if token != TOKEN {
                                send_frame(
                                    &mut write_half,
                                    &GuiReply::Error {
                                        message: format!("{AUTH_ERR}: неверный токен"),
                                    },
                                )
                                .await
                                .ok();
                                return;
                            }
                            if send_frame(&mut write_half, &GuiReply::Ok).await.is_err() {
                                return;
                            }
                            let n = counter.fetch_add(1, Ordering::SeqCst) as usize;
                            for event in script.get(n).cloned().unwrap_or_default() {
                                let value = serde_json::to_value(event).unwrap();
                                let mut buf = BytesMut::new();
                                LengthJsonCodec::<Value>::new()
                                    .encode(value, &mut buf)
                                    .unwrap();
                                if write_half.write_all(&buf).await.is_err() {
                                    return;
                                }
                            }
                            // Сценарий исчерпан — держим соединение (нет
                            // лишних «разрывов»); иначе — рвём, чтобы
                            // клиент переподключился.
                            if n < script.len() {
                                return;
                            }
                            loop {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                            }
                        }
                        // Control-протокол.
                        let reply = match &req {
                            GuiRequest::Hello { token } if token == TOKEN => GuiReply::Ok,
                            GuiRequest::Hello { .. } => {
                                send_frame(
                                    &mut write_half,
                                    &GuiReply::Error {
                                        message: format!("{AUTH_ERR}: неверный токен"),
                                    },
                                )
                                .await
                                .ok();
                                return;
                            }
                            GuiRequest::Stats => {
                                GuiReply::Stats(Box::new(sample_stats()))
                            }
                            GuiRequest::SyncProfiles { .. } => GuiReply::Ok,
                            _ => GuiReply::Error {
                                message: "не поддержано фейком".to_string(),
                            },
                        };
                        let _ = req_tx.send(value);
                        if send_frame(&mut write_half, &reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        FakeServer {
            socket,
            requests: req_rx,
            _dir: dir,
            _task: task,
        }
    }

    /// Hello с верным токеном → соединение; Stats и SyncProfiles
    /// ходят по нему и отвечают (бриф Step 1).
    #[tokio::test]
    async fn hello_then_request_round_trip() {
        let mut server = spawn_fake(vec![]);
        let client = DaemonClient::with_socket(server.socket.clone());

        client.connect(TOKEN).await.expect("Hello принят");

        let reply = client.request(GuiRequest::Stats).await.expect("Stats");
        assert_eq!(reply, GuiReply::Stats(Box::new(sample_stats())));

        client
            .sync_all(vec![FullProfile {
                profile: bottle_core::profile::BottleProfile {
                    id: "office-a".to_string(),
                    name: "Офис А".to_string(),
                    kind: bottle_core::profile::BottleKind::Env,
                    vpn: bottle_core::profile::VpnConfig::Env {
                        protocol: bottle_core::profile::TunnelProtocol::WireGuard,
                        config: "wg.conf".to_string(),
                    },
                    expose: bottle_core::profile::ExposeConfig {
                        socks5: 0,
                        forwards: vec![],
                        dns: None,
                        launch: None,
                    },
                },
                config_text: Some("[Interface]".to_string()),
                credentials: Some(VpnCredentials {
                    username: "ivan".to_string(),
                    password: "s3cret".to_string(),
                }),
            }])
            .await
            .expect("SyncProfiles");

        // Сервер увидел Hello, Stats и SyncProfiles в этом порядке.
        let hello = server.requests.recv().await.expect("Hello у сервера");
        let first = server.requests.recv().await.expect("Stats у сервера");
        let second = server.requests.recv().await.expect("SyncProfiles у сервера");
        assert!(hello.get("Hello").is_some(), "{hello}");
        assert_eq!(
            serde_json::from_value::<GuiRequest>(first).unwrap(),
            GuiRequest::Stats
        );
        assert!(matches!(
            serde_json::from_value::<GuiRequest>(second).unwrap(),
            GuiRequest::SyncProfiles { .. }
        ));
    }

    /// Неверный токен: демон отвечает AUTH и закрывает соединение.
    #[tokio::test]
    async fn connect_with_wrong_token_is_auth_error() {
        let server = spawn_fake(vec![]);
        let client = DaemonClient::with_socket(server.socket.clone());

        let error = client.connect("wrong").await.expect_err("AUTH-отказ");
        assert!(error.starts_with(AUTH_ERR), "{error}");
        // Соединение не сохранилось: запросы отклоняются.
        let error = client
            .request(GuiRequest::Stats)
            .await
            .expect_err("нет соединения");
        assert!(error.contains("нет соединения"), "{error}");
    }

    /// Запрос без connect — понятная ошибка, а не паника.
    #[tokio::test]
    async fn request_without_connect_is_error() {
        let client = DaemonClient::with_socket(PathBuf::from("/tmp/pogrebok-no.sock"));
        let error = client
            .request(GuiRequest::Stats)
            .await
            .expect_err("отказ без соединения");
        assert!(error.contains("нет соединения"), "{error}");
    }

    /// RF#3: разрыв event-соединения → `daemon-disconnected` →
    /// переподключение и продолжение приёма событий (бриф Step 1).
    #[tokio::test]
    async fn event_loop_reconnects_after_drop() {
        let script = vec![vec![sample_event("a")], vec![sample_event("b")]];
        let server = spawn_fake(script);
        let client = DaemonClient::with_socket(server.socket.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
        let emit: Emitter = Arc::new(move |name, payload| {
            let _ = tx.send((name.to_string(), payload));
        });
        let inner = Arc::clone(&client.inner);
        let loop_task = tokio::spawn(run_event_loop(
            inner,
            TOKEN.to_string(),
            emit,
            |_| Duration::from_millis(5),
            5,
        ));

        async fn recv(
            rx: &mut mpsc::UnboundedReceiver<(String, Value)>,
        ) -> (String, Value) {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("событие за таймаут")
                .expect("канал жив")
        }

        let (name, payload) = recv(&mut rx).await;
        assert_eq!(name, EVT_EVENT);
        assert_eq!(
            serde_json::from_value::<DaemonEvent>(payload).unwrap(),
            sample_event("a")
        );

        // Разрыв после первой подписки: GUI узнаёт об этом.
        let (name, _) = recv(&mut rx).await;
        assert_eq!(name, EVT_DISCONNECTED);

        // Переподключились и получили событие второго сценария.
        let (name, payload) = recv(&mut rx).await;
        assert_eq!(name, EVT_EVENT);
        assert_eq!(
            serde_json::from_value::<DaemonEvent>(payload).unwrap(),
            sample_event("b")
        );

        loop_task.abort();
    }

    /// Токен: 64 hex-символа, два поколения не совпадают.
    #[test]
    fn generate_token_is_64_hex_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }
}
