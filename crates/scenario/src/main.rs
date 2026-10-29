//! Dev-драйвер сценариев CI (0.3): говорит с демоном протоколом
//! GUI↔демон (спека §5) вместо снятых команд старого CLI.
//!
//! Команды:
//!   ping  — подключение (с окном ожидания подъёма демона), Hello, Stats;
//!   up    — SyncProfiles из JSON-файла + StartBottle, ждёт событие
//!           `bottle.up` и печатает `SOCKS5=127.0.0.1:<port>`;
//!   down  — StopBottle (ответ демона и есть подтверждение остановки).
//!
//! Транспорт повторяет клиент GUI (`gui/src-tauri/src/daemon_client.rs`):
//! UDS `<data>/daemon.sock` на unix, named pipe `\\.\pipe\pogrebok-daemon`
//! на Windows; кадры `serde_json::Value` через `LengthJsonCodec`.
//!
//! Коды выхода: 0 — успех, 1 — отказ операции/таймаут, 2 — ошибка
//! использования (аргументы).

use std::path::PathBuf;
use std::time::Duration;

use bottle_core::frame::LengthJsonCodec;
#[cfg(windows)]
use bottle_core::ipc::DAEMON_PIPE;
use bottle_core::ipc::{DaemonEvent, FullProfile, GuiReply, GuiRequest, AUTH_ERR};
use bytes::BytesMut;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_stream::StreamExt;
use tokio_util::codec::{Encoder, FramedRead};

/// Таймаут ответа демона на обычные запросы (Hello, Stats, Sync, Stop).
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Таймаут ответа на StartBottle: подъём включает импорт/загрузку
/// аплаенса и установку туннеля — на холодном CI-раннере это минуты.
const START_REPLY_TIMEOUT: Duration = Duration::from_secs(180);

/// Окно ожидания события `bottle.up` после Ok на StartBottle.
const UP_EVENT_TIMEOUT: Duration = Duration::from_secs(180);

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

/// Разобранные аргументы командной строки.
struct Args {
    command: Command,
    data_dir: Option<PathBuf>,
    token: String,
    /// ping: окно ожидания подъёма демона (сек).
    wait: u64,
    /// up: JSON-файл с FullProfile.
    profile: Option<PathBuf>,
    /// down: id бутылки.
    id: Option<String>,
}

enum Command {
    Ping,
    Up,
    Down,
}

#[tokio::main]
async fn main() {
    let code = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => match run(args).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("ошибка: {error}");
                1
            }
        },
        Err(usage) => {
            eprintln!("{usage}");
            2
        }
    };
    std::process::exit(code);
}

async fn run(args: Args) -> Result<(), String> {
    let endpoint = endpoint_of(args.data_dir.as_deref())?;
    match args.command {
        Command::Ping => ping(&endpoint, &args.token, Duration::from_secs(args.wait)).await,
        Command::Up => {
            let profile_path = args
                .profile
                .ok_or_else(|| "up: нужен --profile <json-файл>".to_string())?;
            up(&endpoint, &args.token, &profile_path).await
        }
        Command::Down => {
            let id = args
                .id
                .ok_or_else(|| "down: нужен --id <id бутылки>".to_string())?;
            down(&endpoint, &args.token, &id).await
        }
    }
}

/// Эндпоинт по каталогу данных (unix) или фиксированному пайпу (windows).
fn endpoint_of(data_dir: Option<&std::path::Path>) -> Result<Endpoint, String> {
    #[cfg(unix)]
    {
        let dir = match data_dir {
            Some(dir) => dir.to_path_buf(),
            None => bottle_core::ipc::default_data_dir()
                .ok_or_else(|| "каталог данных недоступен — укажите --data-dir".to_string())?,
        };
        Ok(Endpoint::Socket(bottle_core::ipc::daemon_socket_path(&dir)))
    }
    #[cfg(windows)]
    {
        let _ = data_dir;
        Ok(Endpoint::Pipe(DAEMON_PIPE.to_string()))
    }
}

/// ping: подключение с окном ожидания, Hello, Stats → печать JSON.
async fn ping(endpoint: &Endpoint, token: &str, wait: Duration) -> Result<(), String> {
    let mut conn = connect(endpoint, token, wait).await?;
    match request(&mut conn, &GuiRequest::Stats, REPLY_TIMEOUT).await? {
        GuiReply::Stats(stats) => {
            let json = serde_json::to_string_pretty(&stats)
                .map_err(|error| format!("ошибка кодирования Stats: {error}"))?;
            println!("{json}");
            Ok(())
        }
        GuiReply::Error { message } => Err(message),
        other => Err(format!("неожиданный ответ демона на Stats: {other:?}")),
    }
}

/// up: подписка на события → SyncProfiles → StartBottle → событие
/// `bottle.up` (порт SOCKS5 печатается строкой `SOCKS5=127.0.0.1:<port>`).
///
/// Подписка открывается ДО StartBottle: `bottle.up` эмитится в шину до
/// ответа на start, и без живого подписчика событие потеряется.
async fn up(
    endpoint: &Endpoint,
    token: &str,
    profile_path: &std::path::Path,
) -> Result<(), String> {
    let text = std::fs::read_to_string(profile_path)
        .map_err(|error| format!("не читается {}: {error}", profile_path.display()))?;
    let profile: FullProfile = serde_json::from_str(&text)
        .map_err(|error| format!("битый профиль {}: {error}", profile_path.display()))?;
    let id = profile.profile.id.clone();

    let mut events = subscribe(endpoint, token).await?;

    let mut conn = connect(endpoint, token, Duration::from_secs(5)).await?;
    match request(
        &mut conn,
        &GuiRequest::SyncProfiles {
            profiles: vec![profile],
        },
        REPLY_TIMEOUT,
    )
    .await?
    {
        GuiReply::Ok => {}
        GuiReply::Error { message } => return Err(format!("SyncProfiles: {message}")),
        other => {
            return Err(format!(
                "неожиданный ответ демона на SyncProfiles: {other:?}"
            ))
        }
    }
    match request(
        &mut conn,
        &GuiRequest::StartBottle { id: id.clone() },
        START_REPLY_TIMEOUT,
    )
    .await?
    {
        GuiReply::Ok => {}
        GuiReply::Error { message } => return Err(format!("StartBottle: {message}")),
        other => {
            return Err(format!(
                "неожиданный ответ демона на StartBottle: {other:?}"
            ))
        }
    }

    // StartBottle Ok = туннель поднят; реальный порт — в событии bottle.up.
    let wait = tokio::time::timeout(UP_EVENT_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            match event {
                DaemonEvent::Log(log) if log.event == "bottle.up" && log.ctx["id"] == id => {
                    let port = log.ctx["port"]
                        .as_u64()
                        .ok_or_else(|| "bottle.up без порта".to_string())?;
                    return Ok(port as u16);
                }
                DaemonEvent::Log(log) if log.event == "bottle.failed" && log.ctx["id"] == id => {
                    let reason = log.ctx["reason"].as_str().unwrap_or("без причины");
                    return Err(format!("бутылка {id} не поднялась: {reason}"));
                }
                DaemonEvent::StateChanged {
                    id: event_id,
                    state,
                    socks5: Some(port),
                    ..
                } if event_id == id && state == "up" => return Ok(port),
                _ => {}
            }
        }
        Err("демон закрыл event-поток до bottle.up".to_string())
    })
    .await;
    match wait {
        Ok(Ok(port)) => {
            println!("SOCKS5=127.0.0.1:{port}");
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "нет события bottle.up за {} с",
            UP_EVENT_TIMEOUT.as_secs()
        )),
    }
}

/// down: StopBottle; Ok демона — подтверждение остановки (события
/// остановки в реестре 0.3 нет, TCP-закрытие проверяет сам сценарий CI).
async fn down(endpoint: &Endpoint, token: &str, id: &str) -> Result<(), String> {
    let mut conn = connect(endpoint, token, Duration::from_secs(5)).await?;
    match request(
        &mut conn,
        &GuiRequest::StopBottle { id: id.to_string() },
        REPLY_TIMEOUT,
    )
    .await?
    {
        GuiReply::Ok => {
            println!("STOPPED={id}");
            Ok(())
        }
        GuiReply::Error { message } => Err(format!("StopBottle: {message}")),
        other => Err(format!("неожиданный ответ демона на StopBottle: {other:?}")),
    }
}

/// Подключение + handshake Hello с окном ожидания подъёма демона.
async fn connect(
    endpoint: &Endpoint,
    token: &str,
    window: Duration,
) -> Result<ControlConn, String> {
    let pipe = open(endpoint, window).await?;
    let (read_half, write_half) = tokio::io::split(pipe);
    let mut conn = ControlConn {
        reader: FramedRead::new(read_half, LengthJsonCodec::<Value>::new()),
        writer: write_half,
    };
    send_frame(
        &mut conn.writer,
        &GuiRequest::Hello {
            token: token.into(),
        },
    )
    .await?;
    match read_reply(&mut conn.reader, REPLY_TIMEOUT).await? {
        GuiReply::Ok => Ok(conn),
        GuiReply::Error { message } => Err(message),
        other => Err(format!("неожиданный ответ демона на Hello: {other:?}")),
    }
}

/// Подписка на события демона: Subscribe → ack Ok → канал DaemonEvent.
/// Чтение потока живёт в фоновой задаче; разрыв закрывает канал.
async fn subscribe(
    endpoint: &Endpoint,
    token: &str,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<DaemonEvent>, String> {
    let pipe = open(endpoint, Duration::from_secs(5)).await?;
    let (read_half, mut write_half) = tokio::io::split(pipe);
    send_frame(
        &mut write_half,
        &GuiRequest::Subscribe {
            token: token.into(),
        },
    )
    .await?;
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<Value>::new());
    match read_reply(&mut reader, REPLY_TIMEOUT).await? {
        GuiReply::Ok => {}
        GuiReply::Error { message } => return Err(format!("подписка отклонена: {message}")),
        other => return Err(format!("неожиданный ответ демона на Subscribe: {other:?}")),
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(frame) = reader.next().await {
            match frame {
                Ok(value) => match serde_json::from_value::<DaemonEvent>(value) {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        eprintln!("битый кадр события демона: {error}");
                        return;
                    }
                },
                Err(error) => {
                    eprintln!("ошибка чтения событий: {error}");
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// Полудуплексы control-соединения (кадры Value — нейтральный формат
/// сессии, см. daemon::session).
struct ControlConn {
    reader: FramedRead<ReadHalf<Box<dyn Pipe>>, LengthJsonCodec<Value>>,
    writer: WriteHalf<Box<dyn Pipe>>,
}

/// Запрос по control-соединению с таймаутом ответа.
async fn request(
    conn: &mut ControlConn,
    req: &GuiRequest,
    timeout: Duration,
) -> Result<GuiReply, String> {
    send_frame(&mut conn.writer, req).await?;
    read_reply(&mut conn.reader, timeout).await
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
                // NotFound/ConnectionRefused — демон ещё не поднялся;
                // WouldBlock — ERROR_PIPE_BUSY на Windows (сервер занят).
                let stale = matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::WouldBlock
                );
                if !stale || tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "не удалось подключиться к демону за {} с — запущен ли он?",
                        window.as_secs()
                    ));
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
    timeout: Duration,
) -> Result<GuiReply, String> {
    match tokio::time::timeout(timeout, reader.next()).await {
        Ok(Some(Ok(value))) => {
            serde_json::from_value(value).map_err(|error| format!("битый ответ демона: {error}"))
        }
        Ok(Some(Err(error))) => Err(format!("ошибка обмена с демоном: {error}")),
        Ok(None) => Err("демон закрыл соединение без ответа".to_string()),
        Err(_) => Err(format!("демон не ответил за {} с", timeout.as_secs())),
    }
}

/// Разбор аргументов: `<команда> --token <64 hex> [--data-dir <PATH>]`
/// плюс опции команды (`--wait`, `--profile`, `--id`); формы `--flag=value`
/// тоже допустимы.
fn parse_args<I: Iterator<Item = std::ffi::OsString>>(args: I) -> Result<Args, String> {
    const USAGE: &str = "использование:\n  \
        pogrebok-scenario ping --token <64 hex> [--data-dir <PATH>] [--wait <сек>]\n  \
        pogrebok-scenario up --token <64 hex> --profile <json-файл> [--data-dir <PATH>]\n  \
        pogrebok-scenario down --token <64 hex> --id <id> [--data-dir <PATH>]";
    let mut args: Vec<String> = args.map(|arg| arg.to_string_lossy().into_owned()).collect();
    if args.is_empty() {
        return Err(USAGE.to_string());
    }
    let command = match args.remove(0).as_str() {
        "ping" => Command::Ping,
        "up" => Command::Up,
        "down" => Command::Down,
        other => return Err(format!("неизвестная команда «{other}»\n{USAGE}")),
    };

    let mut data_dir = None;
    let mut token = None;
    let mut wait = 20u64;
    let mut profile = None;
    let mut id = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg, None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline {
                Some(ref value) => Ok(value.clone()),
                None => iter
                    .next()
                    .ok_or_else(|| format!("флагу {name} нужно значение\n{USAGE}")),
            }
        };
        match flag.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--token" => token = Some(value("--token")?),
            "--wait" => {
                wait = value("--wait")?
                    .parse()
                    .map_err(|_| "--wait: нужно число секунд".to_string())?
            }
            "--profile" => profile = Some(PathBuf::from(value("--profile")?)),
            "--id" => id = Some(value("--id")?),
            other => return Err(format!("неизвестный флаг «{other}»\n{USAGE}")),
        }
    }

    let token = token.ok_or_else(|| format!("нужен --token <64 hex>\n{USAGE}"))?;
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "--token: нужны 64 hex-символа ({AUTH_ERR}-гейт демона)"
        ));
    }
    Ok(Args {
        command,
        data_dir,
        token,
        wait,
        profile,
        id,
    })
}
