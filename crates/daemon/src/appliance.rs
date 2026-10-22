//! Импорт WSL-апплайенса, провижининг токена, соединение с агентом.
//!
//! [`ensure_appliance`] идемпотентен: если дистрибуция `bottleforapps`
//! уже импортирован, используется локально сохранённый токен (его
//! отсутствие фатально — восстановить токен нельзя); свежий импорт
//! генерирует токен, записывает его в VM (`/etc/bottle-agent/token`,
//! stdin команды `cat`) и сохраняет локальную копию в хранилище
//! (права 0600). Затем аплаенс будится, по `/sbin/ip -4 -o addr show
//! scope global` узнаётся его адрес в сети WSL NAT (BusyBox-совместимо —
//! `hostname -I` в Alpine отсутствует; путь абсолютный, т.к. PATH у
//! `wsl --exec` минимален) и открывается control-соединение с агентом:
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

/// Декодирует вывод `wsl.exe`, толерантно к UTF-16LE.
///
/// Старый встроенный (inbox) `wsl.exe` игнорирует `WSL_UTF8=1` и пишет
/// свои сообщения (`-l`, `--status`, ошибки импорта) в UTF-16LE — с BOM
/// или без. Современный (Store) wsl и вывод Linux-команд `--exec` —
/// честный UTF-8, поэтому UTF-8 проверяется первым: байты из Linux
/// гарантированно не переинтерпретируются. Эвристика BOM-less UTF-16LE —
/// нулевые байты в потоке (ASCII-символы в UTF-16LE дают `<c> 00`);
/// поток без нулевых байтов, не разобранный как UTF-8, отдаётся lossy.
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        // UTF-16LE с BOM.
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.contains(&0) {
        // Старый wsl.exe: UTF-16LE без BOM. Проверка раньше UTF-8 —
        // ASCII-подобный UTF-16LE формально валиден как UTF-8.
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        // Обычный вывод Linux-команд (--exec) и современного wsl.exe.
        return s.to_string();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

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
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("wsl.exe");
        // Демон может быть запущен из GUI без консоли: без этого флага
        // каждый вызов wsl.exe создавал бы видимое окно консоли.
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                                         // Без WSL_UTF8=1 wsl.exe печатает UTF-16LE.
                                         // WSL_UTF8=1 просит честный UTF-8; старый inbox wsl.exe его
                                         // игнорирует и пишет UTF-16LE — это покрывает decode_wsl_output.
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
            // Сырые байты в debug-журнал: «пустой stderr с кодом 1»
            // на старом wsl.exe (UTF-16LE без BOM) иначе не отличить
            // от «команда не найдена» — hex снимает неоднозначность.
            tracing::debug!(
                args = ?args,
                status = %output.status,
                stderr_hex = %hex(&output.stderr),
                stdout_hex = %hex(&output.stdout),
                "wsl.exe завершился с ошибкой"
            );
            // Детали обычно в stderr, но старые версии wsl.exe пишут
            // «command not found» и т.п. в stdout — тогда берём его,
            // иначе ошибка выходит пустой («код 1: » и тишина).
            let stderr = decode_wsl_output(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                decode_wsl_output(&output.stdout).trim().to_string()
            } else {
                stderr
            };
            return Err(io::Error::other(format!(
                "wsl {} завершился с кодом {}: {}",
                args.join(" "),
                output.status,
                detail
            )));
        }
        Ok(decode_wsl_output(&output.stdout).trim().to_string())
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
    data_root: &Path,
    rootfs_tar: &Path,
    connect_timeout: Duration,
) -> Result<ApplianceHandle, DaemonError> {
    ensure_appliance_with_port(wsl, data_root, rootfs_tar, connect_timeout, AGENT_PORT).await
}

/// `bottle upgrade` (спека §11): снос дистрибуции аплаенса и
/// пере-импорт из `rootfs_tar`. Профили и настройки живут в `<data>`
/// и не трогаются; локальный токен удаляется ДО пере-импорта —
/// иначе ensure увидит «дистрибуция установлена» без токена и
/// ответит «токен утерян». Возвращает версию нового аплаенса (из
/// `HelloAck`; несовместимая версия отвергается handshake).
///
/// Ошибка `--unregister` (дистрибуции нет) не фатальна: upgrade на
/// чистой машине — просто свежий импорт.
pub async fn upgrade_appliance(
    wsl: &dyn WslRunner,
    data_root: &Path,
    rootfs_tar: &Path,
    connect_timeout: Duration,
    force: bool,
) -> Result<String, DaemonError> {
    upgrade_appliance_with_port(wsl, data_root, rootfs_tar, connect_timeout, AGENT_PORT, force).await
}

/// Реализация [`upgrade_appliance`] с настраиваемым портом агента
/// (тестовый шов — как у [`ensure_appliance_with_port`]).
async fn upgrade_appliance_with_port(
    wsl: &dyn WslRunner,
    data_root: &Path,
    rootfs_tar: &Path,
    connect_timeout: Duration,
    port: u16,
    force: bool,
) -> Result<String, DaemonError> {
    // Полный сброс (`upgrade --force`, кнопка «Сбросить аплаенс»): гасим
    // WSL целиком — это снимает блокировки VHD работающей VM, из-за
    // которых unregister молча не отрабатывал (inbox wsl.exe, 0.2.4-0.2.6).
    if force {
        if let Err(error) = wsl.run(&["--shutdown"], None).await {
            tracing::warn!(%error, "wsl --shutdown не удался — продолжаю");
        }
    }
    // Сначала гасим VM и снимаем регистрацию: --unregister по работающей
    // дистрибуции может не отработать (VHD заблокирован), и последующий
    // --import упадёт с невнятным «дистрибуция уже существует» (exit code
    // 0xffffffff). Обязательно проверяем листинг ПОСЛЕ unregister и до
    // удаления локального токена — иначе при неудачном unregister демон
    // встретит старую дистрибуцию без токена и ответит «токен утерян»
    // (наблюдалось на inbox wsl.exe, 0.2.4).
    unregister_appliance(wsl).await?;
    match fs::remove_file(crate::paths::appliance_token_path(data_root)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let handle = ensure_appliance_with_port(wsl, data_root, rootfs_tar, connect_timeout, port).await?;
    Ok(handle.appliance_version)
}

/// Гасит VM и снимает регистрацию дистрибуции аплаенса с проверкой
/// результата: после каждой попытки смотрит `wsl -l -q`. Вторая попытка —
/// с паузой, VM отпускает VHD не мгновенно; третья — после полного
/// `wsl --shutdown` (снимает блокировки VHD на inbox wsl.exe). Ошибка —
/// с ручной инструкцией; вызывающий при этом НЕ удаляет локальный токен.
async fn unregister_appliance(wsl: &dyn WslRunner) -> Result<(), DaemonError> {
    for attempt in 0..3 {
        if attempt == 2 {
            if let Err(error) = wsl.run(&["--shutdown"], None).await {
                tracing::warn!(%error, "wsl --shutdown не удался — продолжаю");
            }
        } else if let Err(error) = wsl.run(&["--terminate", DISTRIBUTION], None).await {
            tracing::warn!(%error, "wsl --terminate не удался (дистрибуции нет?) — продолжаю");
        }
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if let Err(error) = wsl.run(&["--unregister", DISTRIBUTION], None).await {
            tracing::warn!(%error, "wsl --unregister не удался (дистрибуции нет?) — продолжаю");
        }
        match wsl.run(&["-l", "-q"], None).await {
            Ok(listing) if !is_installed(&listing) => return Ok(()),
            Ok(_) => {
                tracing::warn!(attempt, "дистрибуция пережила unregister — повторяю")
            }
            Err(error) => {
                tracing::warn!(attempt, %error, "не удалось проверить листинг после unregister")
            }
        }
    }
    Err(DaemonError::Appliance(format!(
        "не удалось снять регистрацию дистрибуции {DISTRIBUTION} — \
         выполните `wsl --shutdown`, затем `wsl --unregister {DISTRIBUTION}` \
         вручную и повторите `bottle upgrade`"
    )))
}

/// Реализация [`ensure_appliance`] с настраиваемым портом агента:
/// порт 7777 фиксирован протоколом, тесты подменяют его портом
/// локального listener'а.
async fn ensure_appliance_with_port(
    wsl: &dyn WslRunner,
    data_root: &Path,
    rootfs_tar: &Path,
    connect_timeout: Duration,
    port: u16,
) -> Result<ApplianceHandle, DaemonError> {
    let token = provision_token(wsl, data_root, rootfs_tar).await?;
    wake(wsl).await?;
    let vm_ip = appliance_ip(wsl, connect_timeout).await?;
    let stream = connect_with_retries(vm_ip, port, connect_timeout).await?;
    let mut conn = Framed::new(stream, AgentCodec);
    let (_, appliance_version) = handshake(&mut conn, &token, connect_timeout).await?;
    tracing::info!(%vm_ip, port, "соединение с агентом аплаенса установлено");
    Ok(ApplianceHandle {
        vm_ip,
        appliance_version,
        conn,
    })
}

/// Возвращает токен аплаенса.
///
/// Если дистрибуция уже импортирован — токен читается из хранилища
/// (локальной копии); свежая установка генерирует токен, импортирует
/// rootfs-тар, записывает токен в VM и сохраняет локальную копию.
async fn provision_token(
    wsl: &dyn WslRunner,
    data_root: &Path,
    rootfs_tar: &Path,
) -> Result<String, DaemonError> {
    let listing = wsl.run(&["-l", "-q"], None).await?;
    if is_installed(&listing) {
        return local_token(wsl, data_root).await;
    }

    let token = generate_token()?;
    let vm_dir = data_root.join(VM_DIR_NAME);
    if let Err(e) = wsl
        .run(
            &[
                "--import",
                DISTRIBUTION,
                &vm_dir.to_string_lossy(),
                &rootfs_tar.to_string_lossy(),
            ],
            None,
        )
        .await
    {
        // «Дистрибутив уже существует» (код 0xffffffff после прерванной
        // установки): листинг на входе мог быть недоступен/стар — на
        // старом inbox wsl.exe его UTF-16LE вывод раньше вообще не
        // декодировался, и демон бесконечно долбил --import. Страховка:
        // перепроверяем листинг — дистрибутив есть → используем её,
        // а не отправляем пользователя в `wsl --unregister`.
        let relisting = wsl.run(&["-l", "-q"], None).await.unwrap_or_default();
        if is_installed(&relisting) {
            tracing::warn!(
                error = %e,
                "wsl --import отказал, но дистрибутив уже существует — используем её"
            );
            return local_token(wsl, data_root).await;
        }
        return Err(DaemonError::Appliance(format!(
            "не удалось импортировать дистрибуцию {DISTRIBUTION}: {e}. \
             Если она уже существует (например, после прерванной установки), \
             выполните `wsl --unregister {DISTRIBUTION}` и повторите, либо `bottle upgrade`"
        )));
    }
    tracing::info!(
        distribution = DISTRIBUTION,
        "дистрибутив аплаенса импортирован"
    );

    write_token_into_vm(wsl, &token).await?;
    crate::paths::atomic_write(&crate::paths::appliance_token_path(data_root), token.as_bytes())?;
    Ok(token)
}

/// Читает токен из локальной копии хранилища (ветка «дистрибутив уже
/// импортирован»). Локальная копия утеряна (например, upgrade 0.2.4 мог
/// удалить её при неудачном unregister) — токен восстанавливается из VM,
/// где агент хранит его в `/etc/bottle-agent/token`; если и там его нет —
/// [`DaemonError::ApplianceTokenLost`].
async fn local_token(wsl: &dyn WslRunner, data_root: &Path) -> Result<String, DaemonError> {
    match read_local_token(data_root) {
        Ok(token) => Ok(token),
        Err(e) if e.kind() == io::ErrorKind::NotFound => read_token_from_vm(wsl, data_root).await,
        Err(e) => Err(e.into()),
    }
}

/// Восстанавливает утерянную локальную копию токена из VM (агент читает
/// тот же файл, так что handshake пройдёт) и сохраняет её в хранилище.
/// Токен читается выводом `cat` — не аргументом, чтобы не светить секрет
/// в списке процессов хоста.
async fn read_token_from_vm(wsl: &dyn WslRunner, data_root: &Path) -> Result<String, DaemonError> {
    let raw = wsl
        .run(
            &[
                "-d",
                DISTRIBUTION,
                "--exec",
                "/bin/cat",
                "/etc/bottle-agent/token",
            ],
            None,
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, "не удалось прочитать токен из VM");
            DaemonError::ApplianceTokenLost
        })?;
    let token = raw.trim();
    if token.is_empty() {
        tracing::warn!("файл токена в VM пуст");
        return Err(DaemonError::ApplianceTokenLost);
    }
    crate::paths::atomic_write(&crate::paths::appliance_token_path(data_root), token.as_bytes())?;
    tracing::info!("локальная копия токена аплаенса восстановлена из VM");
    Ok(token.to_string())
}

/// Читает локальную копию токена (пробелы по краям игнорируются).
///
/// `NotFound` передаётся наверх как есть — вызывающий превращает его
/// в [`DaemonError::ApplianceTokenLost`].
fn read_local_token(data_root: &Path) -> io::Result<String> {
    let raw = fs::read_to_string(crate::paths::appliance_token_path(data_root))?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "файл токена аплаенса пуст: {}",
                crate::paths::appliance_token_path(data_root).display()
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

/// IP аплаенса в сети WSL NAT — первый global-адрес из
/// `/sbin/ip -4 -o addr show scope global` с ретраями до `timeout`.
///
/// BusyBox-совместимо: аплаенс собран на Alpine, где `hostname` из
/// BusyBox не знает флаг `-I` (GNU inetutils); `iproute2` при этом
/// входит в образ (см. `appliance/Dockerfile`). Фильтр `scope global`
/// отсекает loopback и link-local на стороне VM.
///
/// Путь абсолютный: `wsl --exec` запускает команду без логин-шелла,
/// и PATH там минимален — на части версий WSL без /sbin, где и лежит
/// iproute2. Ретраи нужны на холодном старте: после wake адрес на eth0
/// появляется не мгновенно.
async fn appliance_ip(wsl: &dyn WslRunner, timeout: Duration) -> Result<Ipv4Addr, DaemonError> {
    let args = [
        "-d",
        DISTRIBUTION,
        "--exec",
        "/sbin/ip",
        "-4",
        "-o",
        "addr",
        "show",
        "scope",
        "global",
    ];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match wsl.run(&args, None).await {
            Ok(out) => match parse_global_ipv4(&out) {
                Some(ip) => return Ok(ip),
                None if tokio::time::Instant::now() >= deadline => {
                    return Err(DaemonError::Appliance(format!(
                        "/sbin/ip -4 -o addr show scope global: нет global IPv4 в выводе: {out:?}"
                    )));
                }
                None => {
                    tracing::debug!(%out, "global IPv4 ещё нет, повтор");
                }
            },
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(DaemonError::Appliance(format!(
                        "не удалось получить адрес аплаенса за {timeout:?}: {e}"
                    )));
                }
                tracing::debug!(error = %e, "запрос адреса аплаенса не удался, повтор");
            }
        }
        tokio::time::sleep(CONNECT_RETRY_STEP).await;
    }
}

/// Первый IPv4 из вывода `ip -4 -o addr show scope global`:
/// строки вида "2: eth0    inet 172.25.144.7/20 brd ... scope global eth0".
fn parse_global_ipv4(out: &str) -> Option<Ipv4Addr> {
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        while let Some(part) = parts.next() {
            if part != "inet" {
                continue;
            }
            let Some(raw) = parts.next() else {
                break;
            };
            if let Ok(ip) = raw.split('/').next().unwrap_or("").parse::<Ipv4Addr>() {
                return Some(ip);
            }
        }
    }
    None
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

/// Handshake: `Hello{version, token}` и разбор ответа; возвращает
/// (версия протокола агента, версия аплаенса) из `HelloAck`.
async fn handshake(
    conn: &mut Framed<TcpStream, AgentCodec>,
    token: &str,
    timeout: Duration,
) -> Result<(u32, String), DaemonError> {
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
            appliance_version,
            ..
        } => Ok((PROTOCOL_VERSION, appliance_version)),
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
    /// IP аплаенса в сети WSL NAT (из `ip addr`, см. [`appliance_ip`]).
    pub vm_ip: Ipv4Addr,
    /// Версия аплаенса из `HelloAck` (для сводок и диагностики).
    pub appliance_version: String,
    /// Соединение с агентом; кодек пишет запросы и читает ответы.
    conn: Framed<TcpStream, AgentCodec>,
}

impl ApplianceHandle {
    /// Собирает handle поверх соединения с уже выполненным handshake.
    ///
    /// Тестовый шов менеджера (`manager`): фейк-агенты выполняют
    /// `Hello`/`HelloAck` сами и оборачивают поток этим методом.
    #[cfg(test)]
    pub(crate) fn wrap(vm_ip: Ipv4Addr, stream: TcpStream) -> Self {
        Self::wrap_with_version(vm_ip, stream, "0.1.0-test".to_string())
    }

    /// [`ApplianceHandle::wrap`] с явной версией аплаенса.
    #[cfg(test)]
    pub(crate) fn wrap_with_version(
        vm_ip: Ipv4Addr,
        stream: TcpStream,
        appliance_version: String,
    ) -> Self {
        let _ = stream.set_nodelay(true);
        Self {
            vm_ip,
            appliance_version,
            conn: Framed::new(stream, AgentCodec),
        }
    }

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
        /// Дистрибуция уже импортирован (что вернёт `wsl -l -q`);
        /// `--unregister` сбрасывает флаг.
        installed: std::sync::atomic::AtomicBool,
        /// Сколько первых вызовов `--unregister` НЕ снимают регистрацию
        /// (симптом: VHD заблокирован работающей VM).
        unregister_failures: std::sync::atomic::AtomicUsize,
        calls: Mutex<Vec<Call>>,
        token: SharedToken,
    }

    impl FakeWsl {
        fn new(installed: bool) -> Self {
            Self {
                installed: std::sync::atomic::AtomicBool::new(installed),
                unregister_failures: std::sync::atomic::AtomicUsize::new(0),
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
                return Ok(
                    if self.installed.load(std::sync::atomic::Ordering::SeqCst) {
                        format!("  \u{feff}{DISTRIBUTION}\r\nUbuntu\r\n")
                    } else {
                        "Ubuntu\r\n".to_string()
                    },
                );
            }
            if args.first() == Some(&"--terminate") {
                return Ok(String::new());
            }
            if args.first() == Some(&"--shutdown") {
                return Ok(String::new());
            }
            if args.first() == Some(&"--unregister") {
                use std::sync::atomic::Ordering::SeqCst;
                if self.unregister_failures.load(SeqCst) > 0 {
                    // VHD заблокирован: регистрация не снимается,
                    // wsl при этом отвечает успехом (0xffffffff бывает,
                    // но чаще молчаливая неудача на inbox wsl.exe).
                    self.unregister_failures.fetch_sub(1, SeqCst);
                    return Ok(String::new());
                }
                self.installed.store(false, SeqCst);
                return Ok(String::new());
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
                    // Восстановление токена из VM:
                    // `wsl -d bottleforapps --exec /bin/cat /etc/bottle-agent/token`
                    Some(&"/bin/cat") => {
                        return match self.token.get() {
                            Some(token) => Ok(format!("{token}\n")),
                            None => Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                "cat: /etc/bottle-agent/token: No such file or directory",
                            )),
                        };
                    }
                    // `/sbin/ip -4 -o addr show scope global`: первый
                    // global-адрес — 127.0.0.1, чтобы демон подключился к
                    // фейк-агенту на localhost; вторая строка проверяет,
                    // что берётся именно первый адрес, а не последний.
                    Some(&"/sbin/ip")
                        if args[4..] == ["-4", "-o", "addr", "show", "scope", "global"] =>
                    {
                        return Ok(concat!(
                            "2: eth0    inet 127.0.0.1/20 brd 127.0.0.255 scope global eth0\n",
                            "3: eth1    inet 192.168.77.99/24 brd 192.168.77.255 scope global eth1\n"
                        )
                        .to_string());
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

    /// Каталог данных демона и rootfs-тар во временном каталоге.
    fn test_data() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        crate::paths::open_data_dir(&root).unwrap();
        let rootfs = tmp.path().join("rootfs.tar");
        std::fs::write(&rootfs, b"fake tarball").unwrap();
        (tmp, root, rootfs)
    }

    /// Номер первого вызова, чьи аргументы содержат `needle`.
    fn position(calls: &[Call], needle: &str) -> usize {
        calls
            .iter()
            .position(|c| c.args().iter().any(|a| a.contains(needle)))
            .unwrap_or_else(|| panic!("нет вызова с {needle:?}: {calls:?}"))
    }

    /// Пишет локальный токен в каталог данных и поднимает фейк-агента
    /// (сценарий «дистрибутив уже импортирован»). TempDir возвращается,
    /// чтобы тест держал каталоги живыми на время проверок.
    #[allow(clippy::type_complexity)]
    async fn existing_install(
        scenario: AgentScenario,
    ) -> (
        tempfile::TempDir,
        FakeWsl,
        std::path::PathBuf,
        std::path::PathBuf,
        SocketAddr,
    ) {
        let (tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "local-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.token.set("local-token".to_string());
        let addr = spawn_fake_agent(scenario, wsl.token.clone()).await;
        (tmp, wsl, root, rootfs, addr)
    }

    #[tokio::test]
    async fn fresh_install_imports_and_provisions_token() {
        let (_tmp, root, rootfs) = test_data();
        let wsl = FakeWsl::new(false);
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let handle =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect("свежая установка доходит до соединения");

        assert_eq!(handle.vm_ip, Ipv4Addr::new(127, 0, 0, 1));

        // Локальная копия токена: 64 hex-символа, права 0600.
        let token_path = crate::paths::appliance_token_path(&root);
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
        // в VM → wake → ip addr.
        let calls = wsl.calls();
        assert_eq!(calls[0].args(), ["-l", "-q"], "всё начинается с листинга");

        let import = position(&calls, "--import");
        let import_args = calls[import].args();
        assert_eq!(import_args[1], DISTRIBUTION);
        assert_eq!(
            import_args[2],
            root.join(VM_DIR_NAME).to_str().unwrap()
        );
        assert_eq!(import_args[3], rootfs.to_str().unwrap());

        let token_write = position(&calls, "cat > /etc/bottle-agent/token");
        assert_eq!(
            calls[token_write].stdin,
            Some(format!("{token}\n").into_bytes()),
            "токен пишется в VM через stdin с завершающим \\n"
        );

        let wake = position(&calls, "/bin/true");
        let ip_addr = position(&calls, "scope");
        assert!(import < token_write, "импорт до записи токена: {calls:?}");
        assert!(token_write < wake, "запись токена до wake: {calls:?}");
        assert!(wake < ip_addr, "wake до ip addr: {calls:?}");
        assert_eq!(
            calls[ip_addr].args()[3..],
            ["/sbin/ip", "-4", "-o", "addr", "show", "scope", "global"]
        );

        // Фейк-агент сверил токен Hello с записанным в VM (assert внутри
        // сценария); локальная копия совпадает с записанным в VM.
        assert_eq!(wsl.token.get().as_deref(), Some(token.as_str()));
    }

    /// `bottle upgrade` (Review Focus #2): unregister дистрибуции →
    /// удаление токена → пере-импорт + handshake; версия нового
    /// аплаенса возвращается; токен регенерирован (не старый).
    #[tokio::test]
    async fn upgrade_unregisters_deletes_token_and_reimports() {
        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "old-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.token.set("old-token".to_string());
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let new_version = upgrade_appliance_with_port(
            &wsl,
            &root,
            &rootfs,
            Duration::from_secs(5),
            addr.port(),
            false,
        )
        .await
        .expect("upgrade доходит до handshake");

        assert_eq!(
            new_version, "0.1.0-test",
            "версия из HelloAck нового аплаенса"
        );
        let calls = wsl.calls();
        let terminate = position(&calls, "--terminate");
        let unregister = position(&calls, "--unregister");
        assert_eq!(
            calls[unregister].args(),
            ["--unregister", DISTRIBUTION],
            "сносится только дистрибуция аплаенса"
        );
        assert!(
            terminate < unregister,
            "VM гасится до снятия регистрации: {calls:?}"
        );
        let import = position(&calls, "--import");
        assert!(unregister < import, "unregister до пере-импорта: {calls:?}");

        // Токен регенерирован: старый недействителен, новый совпадает
        // с копией в VM (token-regen путь).
        let local_token = std::fs::read_to_string(crate::paths::appliance_token_path(&root))
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(local_token, "old-token", "токен перегенерирован");
        assert_eq!(wsl.token.get().as_deref(), Some(local_token.as_str()));
    }

    /// Upgrade на машине без дистрибуции: unregister молча пропускается,
    /// выполняется свежий импорт.
    #[tokio::test]
    async fn upgrade_without_existing_distribution_still_imports() {
        let (_tmp, root, rootfs) = test_data();
        let wsl = FakeWsl::new(false);
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let result = upgrade_appliance_with_port(
            &wsl,
            &root,
            &rootfs,
            Duration::from_secs(5),
            addr.port(),
            false,
        )
        .await;

        assert!(
            result.is_ok(),
            "без дистрибуции upgrade = свежий импорт: {result:?}"
        );
    }

    /// unregister по работающей VM молча не снимает регистрацию (VHD
    /// заблокирован, inbox wsl.exe): upgrade повторяет terminate+unregister
    /// и со второй попытки доводит пере-импорт до конца.
    #[tokio::test]
    async fn upgrade_retries_sticky_unregister_and_succeeds() {
        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "old-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.token.set("old-token".to_string());
        wsl.unregister_failures
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let new_version = upgrade_appliance_with_port(
            &wsl,
            &root,
            &rootfs,
            Duration::from_secs(5),
            addr.port(),
            false,
        )
        .await
        .expect("вторая попытка unregister успешна");

        assert_eq!(new_version, "0.1.0-test");
        let calls = wsl.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.args()[0] == "--unregister")
                .count(),
            2,
            "unregister вызван дважды: {calls:?}"
        );
        let new_token = std::fs::read_to_string(crate::paths::appliance_token_path(&root))
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(new_token, "old-token", "токен перегенерирован");
    }

    /// `upgrade --force` (кнопка «Сбросить аплаенс»): WSL гасится целиком
    /// до unregister — так снимаются блокировки VHD.
    #[tokio::test]
    async fn force_upgrade_shuts_down_wsl_before_unregister() {
        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "old-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.token.set("old-token".to_string());
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let new_version = upgrade_appliance_with_port(
            &wsl,
            &root,
            &rootfs,
            Duration::from_secs(5),
            addr.port(),
            true,
        )
        .await
        .expect("force-upgrade доходит до handshake");

        assert_eq!(new_version, "0.1.0-test");
        let calls = wsl.calls();
        let shutdown = position(&calls, "--shutdown");
        let unregister = position(&calls, "--unregister");
        assert!(
            shutdown < unregister,
            "wsl --shutdown до снятия регистрации: {calls:?}"
        );
    }

    /// Дистрибуция пережила все три попытки unregister: upgrade отвечает
    /// понятной ошибкой с ручной инструкцией и НЕ удаляет локальный токен
    /// (0.2.4 удалял — дистрибутив оставалась без токена, «токен утерян»).
    #[tokio::test]
    async fn upgrade_with_stuck_distribution_fails_keeping_local_token() {
        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "old-token\n").unwrap();
        let wsl = FakeWsl::new(true);
        wsl.unregister_failures
            .store(3, std::sync::atomic::Ordering::SeqCst);

        let error =
            upgrade_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), 1, false)
                .await
                .expect_err("застрявшая дистрибуция — ошибка");

        assert!(
            error.to_string().contains("wsl --unregister"),
            "ручная инструкция в тексте: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(crate::paths::appliance_token_path(&root))
                .unwrap()
                .trim(),
            "old-token",
            "локальный токен не удалён при неудачном unregister"
        );
        let calls = wsl.calls();
        assert!(
            !calls.iter().any(|c| c.args()[0] == "--import"),
            "импорт не начинается, пока старая дистрибуция жива: {calls:?}"
        );
    }

    /// Локальная копия токена утеряна (upgrade 0.2.4 при неудачном
    /// unregister): токен читается из VM, handshake проходит, локальная
    /// копия восстанавливается.
    #[tokio::test]
    async fn lost_local_token_is_recovered_from_vm() {
        let (_tmp, root, rootfs) = test_data();
        let wsl = FakeWsl::new(true);
        wsl.token.set("vm-token".to_string());
        let addr = spawn_fake_agent(
            AgentScenario::Happy {
                answer_requests: usize::MAX,
            },
            wsl.token.clone(),
        )
        .await;

        let token = provision_token(&wsl, &root, &rootfs)
            .await
            .expect("токен восстановлен из VM");

        assert_eq!(token, "vm-token");
        assert_eq!(
            std::fs::read_to_string(crate::paths::appliance_token_path(&root))
                .unwrap()
                .trim(),
            "vm-token",
            "локальная копия восстановлена"
        );
        let calls = wsl.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.args().len() > 3 && c.args()[3] == "/bin/cat"),
            "токен прочитан из VM: {calls:?}"
        );
        let _ = addr; // handshake не нужен — проверяется сам provision_token
    }

    /// Ни локальной копии, ни токена в VM (битый импорт) — честный
    /// ApplianceTokenLost с советом `wsl --unregister`.
    #[tokio::test]
    async fn lost_local_token_without_vm_copy_is_token_lost() {
        let (_tmp, root, rootfs) = test_data();
        let wsl = FakeWsl::new(true);

        let error = provision_token(&wsl, &root, &rootfs)
            .await
            .expect_err("токена нет нигде");

        assert!(matches!(error, DaemonError::ApplianceTokenLost));
    }

    #[tokio::test]
    async fn existing_install_reuses_local_token_without_import() {
        let (_tmp, root, rootfs) = test_data();
        const LOCAL_TOKEN: &str = "deadbeefcafe0123";
        std::fs::write(crate::paths::appliance_token_path(&root), format!("{LOCAL_TOKEN}\n")).unwrap();

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
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
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
            calls.iter().any(|c| c.args().contains(&"scope")),
            "ip addr нужен"
        );
    }

    /// WslRunner, отвечающий мусором на `ip addr`: проверка, что
    /// разбор вывода не паникует и возвращает понятную ошибку.
    struct GarbageIpWsl;

    #[async_trait]
    impl WslRunner for GarbageIpWsl {
        async fn run(&self, args: &[&str], _stdin: Option<Vec<u8>>) -> io::Result<String> {
            if args == ["-l", "-q"] {
                return Ok(format!("{DISTRIBUTION}\n"));
            }
            if args.first() == Some(&"-d") && args.get(3) == Some(&"/sbin/ip") {
                // Ни одной строки с global IPv4: только link-local inet6.
                return Ok("2: eth0    inet6 fe80::1/64 scope link".to_string());
            }
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn appliance_ip_without_global_ipv4_is_clear_error() {
        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "local-token\n").unwrap();
        let wsl = GarbageIpWsl;

        // connect_timeout=1с — он же дедлайн ретраев опроса адреса.
        let error =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(1), AGENT_PORT)
                .await
                .expect_err("нет global IPv4 — понятная ошибка");

        assert!(
            matches!(&error, DaemonError::Appliance(message) if message.contains("нет global IPv4")),
            "ожидалась Appliance-ошибка с диагностикой разбора: {error}"
        );
    }

    #[tokio::test]
    async fn installed_without_local_token_reports_lost_token() {
        let (_tmp, root, rootfs) = test_data();
        let wsl = FakeWsl::new(true); // дистрибуция есть, файла токена нет

        let error =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(1), AGENT_PORT)
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
        let (_tmp, wsl, root, rootfs, addr) =
            existing_install(AgentScenario::HelloAckVersion(99)).await;

        let error =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
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
        let (_tmp, wsl, root, rootfs, addr) = existing_install(AgentScenario::ErrorVersion).await;

        let error =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
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
        let (_tmp, wsl, root, rootfs, addr) = existing_install(AgentScenario::ErrorAuth).await;

        let error =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
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
        let (_tmp, wsl, root, rootfs, addr) = existing_install(AgentScenario::Silent).await;

        let timeout = Duration::from_millis(300);
        let started = Instant::now();
        let error = ensure_appliance_with_port(&wsl, &root, &rootfs, timeout, addr.port())
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
        let (_tmp, wsl, root, rootfs, addr) =
            existing_install(AgentScenario::Happy { answer_requests: 3 }).await;

        let mut handle =
            ensure_appliance_with_port(&wsl, &root, &rootfs, Duration::from_secs(5), addr.port())
                .await
                .expect("соединение устанавливается");

        let up = AgentRequest::Up(UpArgs {
            id: "office-a".to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
            dns: None,
            socks5_port: 0,
            forwards: Vec::new(),
            instance: 0,
            username: None,
            password: None,
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

    // ---------------------------------------------------------------
    // decode_wsl_output: UTF-8 / UTF-16LE с BOM / UTF-16LE без BOM.
    // ---------------------------------------------------------------

    /// UTF-16LE-кодировка строки для тестов (опционально с BOM).
    fn utf16le(text: &str, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if bom {
            bytes.extend_from_slice(&[0xFF, 0xFE]);
        }
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn decode_wsl_output_passes_utf8_through() {
        // Вывод Linux-команд (--exec) и современного wsl.exe — не трогаем.
        let text = "2: eth0    inet 172.25.144.7/20 brd … scope global eth0\n";
        assert_eq!(decode_wsl_output(text.as_bytes()), text);
        assert_eq!(decode_wsl_output(b""), "");
    }

    #[test]
    fn decode_wsl_output_reads_utf16le_with_bom() {
        let bytes = utf16le("bottleforapps\r\nUbuntu\r\n", true);
        assert_eq!(decode_wsl_output(&bytes), "bottleforapps\r\nUbuntu\r\n");
    }

    #[test]
    fn decode_wsl_output_reads_utf16le_without_bom() {
        // Старый inbox wsl.exe: UTF-16LE без BOM — маркер «нулевые байты».
        let bytes = utf16le("The distribution already exists.\r\n", false);
        assert_eq!(
            decode_wsl_output(&bytes),
            "The distribution already exists.\r\n"
        );
    }

    #[test]
    fn decode_wsl_output_falls_back_to_lossy_for_garbage() {
        // Не UTF-8 и без нулевых байтов (не UTF-16LE) — lossy, без паники.
        let bytes = [0xE4, 0xB8, 0xAD, 0xFF]; // обрезанный UTF-8 + мусор
        let decoded = decode_wsl_output(&bytes);
        assert!(!decoded.is_empty());
    }

    /// `--import` отказал («дистрибутив уже существует», 0xffffffff), но
    /// перепроверка листинга видит дистрибутив: используем её и читаем
    /// локальный токен вместо совета `wsl --unregister`.
    #[tokio::test]
    async fn import_conflict_with_existing_distribution_falls_back_to_local_token() {
        /// Фейк: первый листинг дистрибутив «не видит» (симптом
        /// inbox-WSL: UTF-16LE вывод не декодировался), `--import`
        /// отказывает «уже существует» (0xffffffff), перепроверка
        /// листинга дистрибутив видит.
        struct AlreadyExistsWsl {
            calls: Mutex<Vec<Call>>,
        }

        #[async_trait]
        impl WslRunner for AlreadyExistsWsl {
            async fn run(&self, args: &[&str], _stdin: Option<Vec<u8>>) -> io::Result<String> {
                let mut calls = self.calls.lock().unwrap();
                calls.push(Call {
                    args: args.iter().map(|s| s.to_string()).collect(),
                    stdin: None,
                });
                if args == ["-l", "-q"] {
                    let first = calls.iter().filter(|c| c.args() == ["-l", "-q"]).count() == 1;
                    return Ok(if first {
                        "Ubuntu\r\n".to_string()
                    } else {
                        format!("{DISTRIBUTION}\r\nUbuntu\r\n")
                    });
                }
                if args.first() == Some(&"--import") {
                    return Err(io::Error::other(
                        "wsl --import завершился с кодом 0xffffffff: уже существует",
                    ));
                }
                panic!("фейковый wsl: неожиданные аргументы {args:?}");
            }
        }

        let (_tmp, root, rootfs) = test_data();
        std::fs::write(crate::paths::appliance_token_path(&root), "local-token\n").unwrap();
        let wsl = AlreadyExistsWsl {
            calls: Mutex::new(Vec::new()),
        };

        // Проверяем provision_token напрямую: важен сам факт, что
        // отказ --import при существующей дистрибуции приводит к чтению
        // локального токена, а не к ошибке с советом `wsl --unregister`.
        let token = provision_token(&wsl, &root, &rootfs)
            .await
            .expect("существующая дистрибуция используется, несмотря на отказ --import");

        assert_eq!(token, "local-token");
        let calls = wsl.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.args()[0] == "--import"),
            "попытка импорта была: {calls:?}"
        );
        assert!(
            calls.iter().filter(|c| c.args() == ["-l", "-q"]).count() >= 2,
            "после отказа --import листинг перепроверяется: {calls:?}"
        );
    }
}
