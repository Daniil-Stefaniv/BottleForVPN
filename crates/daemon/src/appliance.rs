//! Импорт WSL-апплайенса, провижининг токена, соединение с агентом.
//!
//! [`ensure_appliance`] идемпотентен: если дистрибуция `bottleforapps`
//! уже импортирован, используется локально сохранённый токен (его
//! отсутствие фатально — восстановить токен нельзя); свежий импорт
//! генерирует токен, записывает его в VM (`/etc/bottle-agent/token`,
//! stdin команды `cat`) и сохраняет локальную копию в хранилище
//! (права 0600). Затем аплаенс будится, по `hostname -I` узнаётся его
//! адрес в сети WSL NAT и открывается control-соединение с агентом:
//! кадры [`bottle_core::frame`], handshake `Hello`/`HelloAck`.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use bottle_core::frame::{decode_frame, encode_frame, FrameError};
use bottle_core::protocol::{err, AgentReply, AgentRequest, PROTOCOL_VERSION};
use bytes::BytesMut;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, Encoder, Framed};

use crate::error::DaemonError;
use crate::store::Store;

/// Стандартное имя WSL-дистрибутива аплаенса (видно в `wsl -l`).
pub const DISTRIBUTION: &str = "bottleforapps";

/// Порт control-сервера агента внутри аплаенса (спека §4.3).
pub const AGENT_PORT: u16 = 7777;

/// Подкаталог каталога данных демона под файлы дистрибутива
/// (куда `wsl --import` раскладывает VHD).
const VM_DIR_NAME: &str = "appliance-vm";

/// Пауза между попытками TCP-соединения с агентом.
const CONNECT_RETRY_STEP: Duration = Duration::from_millis(500);

/// Таймаут ответа на запрос после handshake: `Up` поднимает туннель
/// целиком (handshake WireGuard/OpenVPN) и длится десятки секунд.
const REPLY_TIMEOUT: Duration = Duration::from_secs(35);

/// Размер токена в байтах до hex-кодирования (hex — 64 символа).
const TOKEN_BYTES: usize = 32;

/// Команда записи токена в VM. Каталог создаётся на случай rootfs
/// без `/etc/bottle-agent` (агент создаёт его лениво, а токен пишется
/// до первого запуска агента).
const TOKEN_WRITE_CMD: &str =
    "mkdir -p /etc/bottle-agent && cat > /etc/bottle-agent/token && chmod 600 /etc/bottle-agent/token";

/// Выполнитель команд WSL (абстракция для тестов).
///
/// `run` выполняет `wsl <args>`, опционально передаёт `stdin`,
/// возвращает stdout без краёв. Ненулевой код возврата — ошибка.
#[async_trait]
pub trait WslRunner: Send + Sync {
    /// Выполняет `wsl <args>` и возвращает stdout (обрезанный по краям).
    async fn run(&self, args: &[&str], stdin: Option<Vec<u8>>) -> io::Result<String>;
}

/// Реальный исполнитель: `wsl.exe` на узле Windows.
#[cfg(windows)]
pub struct RealWsl;

#[cfg(windows)]
impl RealWsl {
    /// Блокирующее выполнение wsl.exe; вызывается из `spawn_blocking`.
    fn run_sync(args: Vec<String>, stdin: Option<Vec<u8>>) -> io::Result<String> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("wsl.exe");
        // Без WSL_UTF8=1 wsl.exe печатает UTF-16LE.
        cmd.env("WSL_UTF8", "1");
        for arg in &args {
            cmd.arg(arg);
        }
        match &stdin {
            Some(_) => cmd.stdin(Stdio::piped()),
            None => cmd.stdin(Stdio::null()),
        };
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::other(format!("не удалось запустить wsl.exe: {e}")))?;

        if let Some(data) = stdin {
            let mut handle = child.stdin.take().expect("stdin piped выше");
            if let Err(e) = handle.write_all(&data) {
                let _ = child.wait();
                return Err(e);
            }
            drop(handle);
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "wsl {} завершился с кодом {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout)
            .map(|out| out.trim().to_string())
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("wsl вернул не-UTF-8 вывод: {e}"),
                )
            })
    }
}

#[cfg(windows)]
#[async_trait]
impl WslRunner for RealWsl {
    async fn run(&self, args: &[&str], stdin: Option<Vec<u8>>) -> io::Result<String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || Self::run_sync(args, stdin))
            .await
            .map_err(|e| io::Error::other(format!("задача wsl.exe отменена: {e}")))?
    }
}

/// Гарантирует, что аплаенс импортирован, токен провижен, агент
/// отвечает: при необходимости импортирует дистрибутив из `rootfs_tar`,
/// будит VM и устанавливает control-соединение (`Hello`/`HelloAck`).
///
/// Соединение с агентом устанавливается с ретраями в течение
/// `connect_timeout` (агент стартует при первом обращении к VM).
pub async fn ensure_appliance(
    wsl: &dyn WslRunner,
    store: &Store,
    rootfs_tar: &Path,
    connect_timeout: Duration,
) -> Result<ApplianceHandle, DaemonError> {
    ensure_appliance_with_port(wsl, store, rootfs_tar, connect_timeout, AGENT_PORT).await
}

/// Реализация [`ensure_appliance`] с настраиваемым портом агента:
/// порт 7777 фиксирован протоколом, тесты подменяют его портом
/// локального listener'а.
async fn ensure_appliance_with_port(
    wsl: &dyn WslRunner,
    store: &Store,
    rootfs_tar: &Path,
    connect_timeout: Duration,
    port: u16,
) -> Result<ApplianceHandle, DaemonError> {
    let token = provision_token(wsl, store, rootfs_tar).await?;
    wake(wsl).await?;
    let vm_ip = appliance_ip(wsl).await?;
    let stream = connect_with_retries(vm_ip, port, connect_timeout).await?;
    let mut conn = Framed::new(stream, AgentCodec);
    handshake(&mut conn, &token, connect_timeout).await?;
    tracing::info!(%vm_ip, port, "соединение с агентом аплаенса установлено");
    Ok(ApplianceHandle { vm_ip, conn })
}

/// Возвращает токен аплаенса.
///
/// Если дистрибуция уже импортирован — токен читается из хранилища
/// (локальной копии); свежая установка генерирует токен, импортирует
/// rootfs-тар, записывает токен в VM и сохраняет локальную копию.
async fn provision_token(
    wsl: &dyn WslRunner,
    store: &Store,
    rootfs_tar: &Path,
) -> Result<String, DaemonError> {
    let listing = wsl.run(&["-l", "-q"], None).await?;
    if is_installed(&listing) {
        return match read_local_token(store) {
            Ok(token) => Ok(token),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(DaemonError::ApplianceTokenLost),
            Err(e) => Err(e.into()),
        };
    }

    let token = generate_token()?;
    let vm_dir = store.data_root().join(VM_DIR_NAME);
    wsl.run(
        &[
            "--import",
            DISTRIBUTION,
            &vm_dir.to_string_lossy(),
            &rootfs_tar.to_string_lossy(),
        ],
        None,
    )
    .await?;
    tracing::info!(
        distribution = DISTRIBUTION,
        "дистрибутив аплаенса импортирован"
    );

    write_token_into_vm(wsl, &token).await?;
    crate::store::atomic_write(&store.appliance_token_path(), token.as_bytes())?;
    Ok(token)
}

/// Читает локальную копию токена (пробелы по краям игнорируются).
///
/// `NotFound` передаётся наверх как есть — вызывающий превращает его
/// в [`DaemonError::ApplianceTokenLost`].
fn read_local_token(store: &Store) -> io::Result<String> {
    let raw = fs::read_to_string(store.appliance_token_path())?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "файл токена аплаенса пуст: {}",
                store.appliance_token_path().display()
            ),
        ));
    }
    Ok(token.to_string())
}

/// Записывает токен в VM (`/etc/bottle-agent/token`, права 0600).
///
/// Токен передаётся через stdin команды `cat` — не через аргументы,
/// чтобы не светить секрет в списке процессов.
async fn write_token_into_vm(wsl: &dyn WslRunner, token: &str) -> Result<(), DaemonError> {
    wsl.run(
        &[
            "-d",
            DISTRIBUTION,
            "--exec",
            "/bin/sh",
            "-c",
            TOKEN_WRITE_CMD,
        ],
        Some(format!("{token}\n").into_bytes()),
    )
    .await?;
    Ok(())
}

/// Будит аплаенс: любой запуск команды поднимает VM с агентом.
async fn wake(wsl: &dyn WslRunner) -> Result<(), DaemonError> {
    wsl.run(&["-d", DISTRIBUTION, "--exec", "/bin/true"], None)
        .await?;
    Ok(())
}

/// IP аплаенса в сети WSL NAT — первый адрес из `hostname -I`.
async fn appliance_ip(wsl: &dyn WslRunner) -> Result<Ipv4Addr, DaemonError> {
    let out = wsl
        .run(&["-d", DISTRIBUTION, "--exec", "hostname", "-I"], None)
        .await?;
    let first = out.split_whitespace().next().ok_or_else(|| {
        DaemonError::Appliance(format!("hostname -I вернул пустой вывод: {out:?}"))
    })?;
    first
        .parse::<Ipv4Addr>()
        .map_err(|_| DaemonError::Appliance(format!("hostname -I вернул не IPv4: {first}")))
}

/// Подключается к агенту с ретраями весь `timeout` (шаг 500 мс):
/// агент стартует при первом обращении к VM и не сразу слушает порт.
async fn connect_with_retries(
    vm_ip: Ipv4Addr,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, DaemonError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match TcpStream::connect((vm_ip, port)).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(DaemonError::ApplianceTimeout { timeout });
                }
                tracing::debug!(error = %e, "агент ещё не слушает, повтор через 500 мс");
                tokio::time::sleep(CONNECT_RETRY_STEP).await;
            }
        }
    }
}

/// Handshake: `Hello{version, token}` и разбор ответа.
async fn handshake(
    conn: &mut Framed<TcpStream, AgentCodec>,
    token: &str,
    timeout: Duration,
) -> Result<(), DaemonError> {
    write_request(
        conn,
        AgentRequest::Hello {
            version: PROTOCOL_VERSION,
            token: token.to_string(),
        },
    )
    .await?;
    match read_reply(conn, timeout).await? {
        AgentReply::HelloAck {
            version: PROTOCOL_VERSION,
            ..
        } => Ok(()),
        AgentReply::HelloAck { .. } => Err(DaemonError::ApplianceIncompatible),
        AgentReply::Error { code, .. } if code == err::VERSION => {
            Err(DaemonError::ApplianceIncompatible)
        }
        AgentReply::Error { code, .. } if code == err::AUTH => Err(DaemonError::ApplianceTokenLost),
        AgentReply::Error { code, message } => Err(DaemonError::Appliance(format!(
            "агент отверг Hello ({code}): {message}"
        ))),
        other => Err(DaemonError::Appliance(format!(
            "неожиданный ответ агента на Hello: {other:?}"
        ))),
    }
}

/// Кодирует и отправляет запрос агенту (fallible-путь Encoder, как в
/// agent::control::send_reply — без Sink).
async fn write_request(
    conn: &mut Framed<TcpStream, AgentCodec>,
    req: AgentRequest,
) -> Result<(), DaemonError> {
    let mut buf = BytesMut::new();
    conn.codec_mut()
        .encode(req, &mut buf)
        .map_err(|e| DaemonError::Appliance(format!("не удалось закодировать запрос: {e}")))?;
    conn.get_mut()
        .write_all(&buf)
        .await
        .map_err(|e| DaemonError::Appliance(format!("не удалось отправить запрос агенту: {e}")))?;
    Ok(())
}

/// Читает следующий ответ агента с тайм-аутом.
async fn read_reply(
    conn: &mut Framed<TcpStream, AgentCodec>,
    timeout: Duration,
) -> Result<AgentReply, DaemonError> {
    match tokio::time::timeout(timeout, conn.next()).await {
        Err(_) => Err(DaemonError::ApplianceTimeout { timeout }),
        Ok(None) => Err(DaemonError::Appliance(
            "агент закрыл соединение".to_string(),
        )),
        Ok(Some(Ok(reply))) => Ok(reply),
        Ok(Some(Err(e))) => Err(DaemonError::Appliance(format!(
            "некорректный кадр от агента: {e}"
        ))),
    }
}

/// Установленное соединение с агентом аплаенса.
///
/// Запросы уходят кадрами [`bottle_core::frame`], ответы читаются
/// последовательно (контрол-протокол — strict request-response).
pub struct ApplianceHandle {
    /// IP аплаенса в сети WSL NAT (из `hostname -I`).
    pub vm_ip: Ipv4Addr,
    /// Соединение с агентом; кодек пишет запросы и читает ответы.
    conn: Framed<TcpStream, AgentCodec>,
}

impl ApplianceHandle {
    /// Отправляет запрос и ждёт ответ на него.
    ///
    /// Таймаут — [`REPLY_TIMEOUT`]: handshake-подобные `Up` длинные.
    pub async fn send(&mut self, req: AgentRequest) -> Result<AgentReply, DaemonError> {
        write_request(&mut self.conn, req).await?;
        read_reply(&mut self.conn, REPLY_TIMEOUT).await
    }

    /// Жив ли агент: `Status` без id должен вернуться `Status`.
    pub async fn alive(&mut self) -> bool {
        matches!(
            self.send(AgentRequest::Status { id: None }).await,
            Ok(AgentReply::Status { .. })
        )
    }
}

/// Debug без соединения (Framed не Debug): только IP аплаенса.
impl std::fmt::Debug for ApplianceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplianceHandle")
            .field("vm_ip", &self.vm_ip)
            .finish()
    }
}

/// Кодек контроль-канала на стороне демона: кодирует запросы
/// [`AgentRequest`], декодирует ответы [`AgentReply`].
///
/// `LengthJsonCodec<T>` универсален по одному типу сообщения, поэтому
/// направление задаёт эта обёртка (кадры — `bottle_core::frame`).
struct AgentCodec;

impl Encoder<AgentRequest> for AgentCodec {
    type Error = FrameError;

    fn encode(&mut self, item: AgentRequest, dst: &mut BytesMut) -> Result<(), FrameError> {
        // encode_frame не падает для типов протокола (см. frame.rs).
        dst.extend_from_slice(&encode_frame(&item));
        Ok(())
    }
}

impl Decoder for AgentCodec {
    type Item = AgentReply;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<AgentReply>, FrameError> {
        decode_frame(src)
    }
}

/// Есть ли дистрибутив аплаенса в выводе `wsl -l -q`.
///
/// Строки чистятся от пробелов, `\r` и BOM (wsl.exe добавляет её к
/// первому столбцу); сравнение точное, чтобы `my-bottleforapps-2`
/// не считался установленным аплаенсом.
fn is_installed(listing: &str) -> bool {
    listing.lines().any(|line| {
        line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}') == DISTRIBUTION
    })
}

/// Генерирует токен авторизации: 32 случайных байта → hex (64 символа).
///
/// Источник энтропии — системный (`getrandom`: на unix это
/// getentropy/urandom — тот же паттерн, что в `agent::token`; на
/// Windows — RtlGenRandom).
fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| io::Error::other(format!("не удалось получить случайные байты: {e}")))?;
    Ok(hex(&bytes))
}

/// Hex-кодирует байты (нижний регистр).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use bottle_core::frame::LengthJsonCodec;
    use bottle_core::profile::TunnelProtocol;
    use bottle_core::protocol::UpArgs;
    use tokio::net::tcp::OwnedWriteHalf;
    use tokio::net::TcpListener;
    use tokio_util::codec::FramedRead;

    use super::*;
    use crate::store::Store;

    /// Один вызов `wsl`: аргументы и stdin.
    #[derive(Debug, Clone)]
    struct Call {
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    }

    impl Call {
        /// Аргументы вызова как &[str] для сравнения.
        fn args(&self) -> Vec<&str> {
            self.args.iter().map(String::as_str).collect()
        }
    }

    /// Токен, «записанный в VM» фейком, — фейк-агент сверяет с ним Hello.
    #[derive(Clone, Default)]
    struct SharedToken(Arc<Mutex<Option<String>>>);

    impl SharedToken {
        fn set(&self, token: String) {
            *self.0.lock().unwrap() = Some(token);
        }

        fn get(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Фейковый WslRunner: ведёт журнал вызовов, отвечает фиксированно.
    struct FakeWsl {
        /// Дистрибуция уже импортирован (что вернёт `wsl -l -q`).
        installed: bool,
        calls: Mutex<Vec<Call>>,
        token: SharedToken,
    }

    impl FakeWsl {
        fn new(installed: bool) -> Self {
            Self {
                installed,
                calls: Mutex::new(Vec::new()),
                token: SharedToken::default(),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WslRunner for FakeWsl {
        async fn run(&self, args: &[&str], stdin: Option<Vec<u8>>) -> io::Result<String> {
            let call = Call {
                args: args.iter().map(|s| s.to_string()).collect(),
                stdin: stdin.clone(),
            };
            self.calls.lock().unwrap().push(call.clone());

            let args = call.args();
            if args == ["-l", "-q"] {
                return Ok(if self.installed {
                    format!("  \u{feff}{DISTRIBUTION}\r\nUbuntu\r\n")
                } else {
                    "Ubuntu\r\n".to_string()
                });
            }
            if args.first() == Some(&"--import") {
                return Ok(String::new());
            }
            if args.first() == Some(&"-d") && args.get(2) == Some(&"--exec") {
                // Команда после `-d <dist> --exec` и её аргументы.
                match args.get(3) {
                    Some(&"/bin/sh") => {
                        assert_eq!(
                            args.get(4),
                            Some(&"-c"),
                            "скрипт передаётся через sh -c: {args:?}"
                        );
                        let script = args[5];
                        if script.contains("cat > /etc/bottle-agent/token") {
                            let data = stdin.expect("токен передаётся через stdin");
                            let token = String::from_utf8(data).expect("токен — UTF-8");
                            self.token.set(token.trim().to_string());
                            return Ok(String::new());
                        }
                    }
                    // wake: `wsl -d bottleforapps --exec /bin/true`
                    Some(&"/bin/true") => return Ok(String::new()),
                    Some(&"hostname") if args[4..] == ["-I"] => {
                        return Ok("127.0.0.1 192.168.77.99\n".to_string());
                    }
                    _ => {}
                }
            }
            panic!("фейковый wsl: неожиданные аргументы {args:?}");
        }
    }

    /// Сценарий фейк-агента после получения Hello.
    enum AgentScenario {
        /// HelloAck и осмысленные ответы на запросы; после
        /// `answer_requests` обработанных запросов соединение закрывается.
        Happy { answer_requests: usize },
        /// HelloAck с указанной версией протокола.
        HelloAckVersion(u32),
        /// Ответ Error VERSION.
        ErrorVersion,
        /// Ответ Error AUTH.
        ErrorAuth,
        /// Прочитать Hello и не отвечать.
        Silent,
    }

    /// Поднимает фейк-агента на 127.0.0.1:0, говорящего протоколом
    /// кадр-JSON (тот же LengthJsonCodec, что у настоящего агента).
    async fn spawn_fake_agent(scenario: AgentScenario, token: SharedToken) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("демон подключится");
            stream.set_nodelay(true).expect("nodelay");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = FramedRead::new(read_half, LengthJsonCodec::<AgentRequest>::new());
            let mut codec = LengthJsonCodec::<AgentReply>::new();

            // Первым сообщением — Hello с тем же токеном.
            let hello = reader
                .next()
                .await
                .expect("кадр Hello")
                .expect("валидный Hello");
            let AgentRequest::Hello {
                version,
                token: hello_token,
            } = hello
            else {
                panic!("первым сообщением ожидается Hello, получено {hello:?}");
            };
            assert_eq!(version, PROTOCOL_VERSION, "демон шлёт текущую версию");
            if let Some(expected) = token.get() {
                assert_eq!(
                    hello_token, expected,
                    "токен Hello должен совпадать с провижененным"
                );
            }

            match scenario {
                AgentScenario::HelloAckVersion(v) => {
                    send_reply(
                        &mut write_half,
                        &mut codec,
                        AgentReply::HelloAck {
                            version: v,
                            appliance_version: "0.1.0-test".to_string(),
                            vm_ip: Ipv4Addr::LOCALHOST,
                        },
                    )
                    .await;
                }
                AgentScenario::ErrorVersion => {
                    send_reply(
                        &mut write_half,
                        &mut codec,
                        AgentReply::Error {
                            code: err::VERSION.to_string(),
                            message: "версия не совпадает".to_string(),
                        },
                    )
                    .await;
                }
                AgentScenario::ErrorAuth => {
                    send_reply(
                        &mut write_half,
                        &mut codec,
                        AgentReply::Error {
                            code: err::AUTH.to_string(),
                            message: "неверный токен".to_string(),
                        },
                    )
                    .await;
                }
                AgentScenario::Silent => {
                    std::future::pending::<()>().await;
                }
                AgentScenario::Happy { answer_requests } => {
                    send_reply(
                        &mut write_half,
                        &mut codec,
                        AgentReply::HelloAck {
                            version: PROTOCOL_VERSION,
                            appliance_version: "0.1.0-test".to_string(),
                            vm_ip: Ipv4Addr::LOCALHOST,
                        },
                    )
                    .await;
                    for _ in 0..answer_requests {
                        let Some(Ok(req)) = reader.next().await else {
                            return; // демон отключился
                        };
                        let reply = match req {
                            AgentRequest::Up(args) => AgentReply::Ready { id: args.id },
                            AgentRequest::Status { .. } => AgentReply::Status {
                                bottles: Vec::new(),
                            },
                            AgentRequest::Down { .. } | AgentRequest::Shutdown => AgentReply::Ok,
                            AgentRequest::Hello { .. } => AgentReply::Error {
                                code: err::BAD_REQUEST.to_string(),
                                message: "повторный Hello".to_string(),
                            },
                        };
                        send_reply(&mut write_half, &mut codec, reply).await;
                    }
                    let _ = write_half.shutdown().await;
                }
            }
        });
        addr
    }

    /// Отправляет ответ фейк-агента (fallible-путь Encoder, как в
    /// agent::control::send_reply).
    async fn send_reply(
        write_half: &mut OwnedWriteHalf,
        codec: &mut LengthJsonCodec<AgentReply>,
        reply: AgentReply,
    ) {
        let mut buf = BytesMut::new();
        codec.encode(reply, &mut buf).expect("кодирование ответа");
        write_half.write_all(&buf).await.expect("отправка ответа");
    }

    /// Хранилище и rootfs-тар во временном каталоге.
    fn test_store() -> (tempfile::TempDir, Store, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        let rootfs = tmp.path().join("rootfs.tar");
        std::fs::write(&rootfs, b"fake tarball").unwrap();
        (tmp, store, rootfs)
    }

    /// Номер первого вызова, чьи аргументы содержат `needle`.
    fn position(calls: &[Call], needle: &str) -> usize {
        calls
            .iter()
            .position(|c| c.args().iter().any(|a| a.contains(needle)))
            .unwrap_or_else(|| panic!("нет вызова с {needle:?}: {calls:?}"))
    }

    /// Пишет локальный токен в хранилище и поднимает фейк-агента
    /// (сценарий «дистрибутив уже импортирован»). TempDir возвращается,
    /// чтобы тест держал каталоги живыми на время проверок.
    #[allow(clippy::type_complexity)]
    async fn existing_install(
        scenario: AgentScenario,
    ) -> (
        tempfile::TempDir,
        FakeWsl,
        Store,
        std::path::PathBuf,
        SocketAddr,
    ) {
        let (tmp, store, rootfs) = test_store();
        std::fs::write(store.appliance_token_path(), "local-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.token.set("local-token".to_string());
        let addr = spawn_fake_agent(scenario, wsl.token.clone()).await;
        (tmp, wsl, store, rootfs, addr)
    }

    #[tokio::test]
    async fn fresh_install_imports_and_provisions_token() {
        let (_tmp, store, rootfs) = test_store();
        let wsl = FakeWsl::new(false);
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let handle =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect("свежая установка доходит до соединения");

        assert_eq!(handle.vm_ip, Ipv4Addr::new(127, 0, 0, 1));

        // Локальная копия токена: 64 hex-символа, права 0600.
        let token_path = store.appliance_token_path();
        let token = std::fs::read_to_string(&token_path)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(token.len(), 64, "токен — hex от 32 байт: {token}");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "токен не hex: {token}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "токен — секрет, права 0600");
        }

        // Последовательность вызовов: листинг → импорт → запись токена
        // в VM → wake → hostname.
        let calls = wsl.calls();
        assert_eq!(calls[0].args(), ["-l", "-q"], "всё начинается с листинга");

        let import = position(&calls, "--import");
        let import_args = calls[import].args();
        assert_eq!(import_args[1], DISTRIBUTION);
        assert_eq!(
            import_args[2],
            store.data_root().join(VM_DIR_NAME).to_str().unwrap()
        );
        assert_eq!(import_args[3], rootfs.to_str().unwrap());

        let token_write = position(&calls, "cat > /etc/bottle-agent/token");
        assert_eq!(
            calls[token_write].stdin,
            Some(format!("{token}\n").into_bytes()),
            "токен пишется в VM через stdin с завершающим \\n"
        );

        let wake = position(&calls, "/bin/true");
        let hostname = position(&calls, "hostname");
        assert!(import < token_write, "импорт до записи токена: {calls:?}");
        assert!(token_write < wake, "запись токена до wake: {calls:?}");
        assert!(wake < hostname, "wake до hostname -I: {calls:?}");
        assert_eq!(calls[hostname].args()[3..], ["hostname", "-I"]);

        // Фейк-агент сверил токен Hello с записанным в VM (assert внутри
        // сценария); локальная копия совпадает с записанным в VM.
        assert_eq!(wsl.token.get().as_deref(), Some(token.as_str()));
    }

    #[tokio::test]
    async fn existing_install_reuses_local_token_without_import() {
        let (_tmp, store, rootfs) = test_store();
        const LOCAL_TOKEN: &str = "deadbeefcafe0123";
        std::fs::write(store.appliance_token_path(), format!("{LOCAL_TOKEN}\n")).unwrap();

        let wsl = FakeWsl::new(true);
        wsl.token.set(LOCAL_TOKEN.to_string());
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let handle =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect("повторный запуск переиспользует дистрибутив");

        assert_eq!(handle.vm_ip, Ipv4Addr::new(127, 0, 0, 1));

        let calls = wsl.calls();
        assert_eq!(calls[0].args(), ["-l", "-q"]);
        assert!(
            calls.iter().all(|c| !c.args().contains(&"--import")),
            "дистрибутив уже импортирован, --import не нужен: {calls:?}"
        );
        assert!(
            calls.iter().all(|c| c
                .args()
                .iter()
                .all(|a| !a.contains("/etc/bottle-agent/token"))),
            "токен уже провижен, повторная запись не нужна: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.args().contains(&"/bin/true")),
            "wake нужен"
        );
        assert!(
            calls.iter().any(|c| c.args().contains(&"hostname")),
            "hostname -I нужен"
        );
    }

    #[tokio::test]
    async fn installed_without_local_token_reports_lost_token() {
        let (_tmp, store, rootfs) = test_store();
        let wsl = FakeWsl::new(true); // дистрибуция есть, файла токена нет

        let error =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(1), AGENT_PORT)
                .await
                .expect_err("токен утерян");

        assert!(matches!(error, DaemonError::ApplianceTokenLost));
        assert!(
            error.to_string().contains("wsl --unregister"),
            "подсказка восстановления: {error}"
        );
    }

    #[tokio::test]
    async fn hello_ack_with_mismatched_version_demands_upgrade() {
        let (_tmp, wsl, store, rootfs, addr) =
            existing_install(AgentScenario::HelloAckVersion(99)).await;

        let error =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect_err("несовместимая версия");

        assert!(
            matches!(error, DaemonError::ApplianceIncompatible),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("обновление"),
            "подсказка обновления: {error}"
        );
    }

    #[tokio::test]
    async fn agent_version_error_demands_upgrade() {
        let (_tmp, wsl, store, rootfs, addr) = existing_install(AgentScenario::ErrorVersion).await;

        let error =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect_err("агент отверг версию");

        assert!(
            matches!(error, DaemonError::ApplianceIncompatible),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("обновление"),
            "подсказка обновления: {error}"
        );
    }

    #[tokio::test]
    async fn agent_auth_error_reports_lost_token() {
        let (_tmp, wsl, store, rootfs, addr) = existing_install(AgentScenario::ErrorAuth).await;

        let error =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect_err("агент отверг токен");

        assert!(
            matches!(error, DaemonError::ApplianceTokenLost),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("wsl --unregister"),
            "подсказка восстановления: {error}"
        );
    }

    #[tokio::test]
    async fn silent_agent_times_out() {
        let (_tmp, wsl, store, rootfs, addr) = existing_install(AgentScenario::Silent).await;

        let timeout = Duration::from_millis(300);
        let started = Instant::now();
        let error = ensure_appliance_with_port(&wsl, &store, &rootfs, timeout, addr.port())
            .await
            .expect_err("агент молчит");
        let elapsed = started.elapsed();

        assert!(
            matches!(error, DaemonError::ApplianceTimeout { .. }),
            "{error:?}"
        );
        assert!(
            elapsed >= timeout,
            "тайм-аут выжидается целиком, прошло {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "тайм-аут не зависает, прошло {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn send_roundtrips_and_alive_detects_closed_connection() {
        // HelloAck + Ready + Status + Status(alive), затем соединение
        // закрывается фейк-агентом.
        let (_tmp, wsl, store, rootfs, addr) =
            existing_install(AgentScenario::Happy { answer_requests: 3 }).await;

        let mut handle =
            ensure_appliance_with_port(&wsl, &store, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect("соединение устанавливается");

        let up = AgentRequest::Up(UpArgs {
            id: "office-a".to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
            dns: None,
            socks5_port: 0,
            forwards: Vec::new(),
        });
        assert_eq!(
            handle.send(up).await.expect("Up отвечает"),
            AgentReply::Ready {
                id: "office-a".to_string()
            }
        );
        assert_eq!(
            handle
                .send(AgentRequest::Status { id: None })
                .await
                .expect("Status отвечает"),
            AgentReply::Status {
                bottles: Vec::new()
            }
        );
        assert!(handle.alive().await, "живой агент отвечает на Status");

        // Фейк-агент закрыл соединение после третьего запроса.
        assert!(!handle.alive().await, "закрытое соединение не alive");
    }

    #[test]
    fn is_installed_matches_distribution_line_only() {
        assert!(is_installed(&format!(
            "Ubuntu\n{DISTRIBUTION}\ndocker-desktop\n"
        )));
        assert!(is_installed(&format!("\u{feff}  {DISTRIBUTION}\r\n")));
        assert!(!is_installed("Ubuntu\ndocker-desktop\n"));
        assert!(!is_installed(&format!("my-{DISTRIBUTION}-2\n")));
        assert!(!is_installed(""));
    }

    #[test]
    fn generated_token_is_hex64_and_unique() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();

        assert_eq!(first.len(), 64, "{first}");
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "токен не hex: {first}"
        );
        assert_ne!(first, second, "каждая установка получает свой токен");
    }
}
