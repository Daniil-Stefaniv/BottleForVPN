//! `bottle` — тонкий CLI-клиент демона BottleForApps (спека §4.4).
//!
//! Вся бизнес-логика живёт в демоне: CLI разбирает аргументы (clap),
//! отправляет [`CliRequest`] по named pipe `\\.\pipe\bottled` и рендерит
//! [`CliReply`] в русский вывод. Рендер вынесен в [`render`] и покрыт
//! unit-тестами на любой платформе; транспорт — только на Windows.
//!
//! Флаг `--json` (M4, рулинг D1) переключает вывод в машинный режим:
//! stdout несёт сериализованный `CliReply`, ошибки — `{"Error":…}`,
//! коды выхода 0/1/2; этим режимом пользуется GUI как сайдкар-подпроцесс.

mod render;

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bottle_core::ipc::{CliReply, CliRequest};
use bottle_core::profile::{
    BottleKind, BottleProfile, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig,
};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "bottle",
    about = "BottleForApps — управление бутылками",
    version,
    disable_version_flag = true
)]
struct Cli {
    /// Машинный режим для GUI: ответы печатаются как JSON (CliReply)
    /// в stdout, коды выхода 0/1/2 (2 — ошибка разбора аргументов).
    #[arg(long, global = true)]
    json: bool,
    /// Показать версию и выйти (`--version --json` → {"version":"…"}).
    #[arg(long, global = true)]
    version: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

/// `Create` — самый крупный вариант (флаги создания профиля);
/// command-перечисление обрабатывается один раз при старте,
/// боксирование вариативного размера не даёт выгоды — допускаем.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Список бутылок в хранилище
    List,
    /// Показать профиль и состояние бутылки
    Show {
        /// Идентификатор бутылки
        id: String,
    },
    /// Поднять бутылку (туннель + SOCKS5 + пробросы)
    Start {
        /// Идентификатор бутылки
        id: String,
    },
    /// Опустить бутылку
    Stop {
        /// Идентификатор бутылки
        id: String,
    },
    /// Состояние поднятых бутылок
    Status,
    /// Диагностика окружения (WSL, аплаенс, служба, порты)
    Doctor,
    /// Создать бутылку из профиля
    Create(CreateArgs),
    /// Удалить бутылку (профиль и конфиг из хранилища)
    Delete {
        /// Идентификатор бутылки
        id: String,
    },
    /// Журналы диагностики
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
    /// Маршруты host-бутылок
    Routes {
        #[command(subcommand)]
        command: RoutesCommand,
    },
    /// Пере-импорт аплаенса (снос дистрибуции WSL + импорт из rootfs)
    Upgrade,
}

/// Подкоманды `bottle routes`.
#[derive(Debug, Subcommand)]
enum RoutesCommand {
    /// Снять сиротские маршруты через службу bottle-svc
    Cleanup,
}

/// Аргументы `bottle create` (спека §5.1). Обязательность полей по
/// типу бутылки (env — protocol/config, host — vpn/endpoints)
/// валидирует демон — единый источник правил в `bottle_core::profile`.
#[derive(Debug, clap::Args)]
struct CreateArgs {
    /// Идентификатор бутылки ([a-z0-9-])
    id: String,
    /// Человекочитаемое имя
    #[arg(long)]
    name: String,
    /// Тип бутылки: env — изолирующее окружение WSL,
    /// host — фирменный клиент на хосте
    #[arg(long, value_enum)]
    kind: KindArg,
    /// Протокол туннеля (для kind=env)
    #[arg(long, value_enum)]
    protocol: Option<ProtocolArg>,
    /// Путь к файлу конфигурации туннеля (для kind=env; копируется
    /// в хранилище)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Путь к фирменному VPN-клиенту (для kind=host)
    #[arg(long)]
    vpn: Option<String>,
    /// Аргументы запуска клиента (для kind=host)
    #[arg(long)]
    vpn_args: Option<String>,
    /// Endpoint VPN (для kind=host; повторяется для каждого адреса)
    #[arg(long = "endpoint")]
    endpoints: Vec<String>,
    /// Имя интерфейса клиента (для kind=host; по умолчанию —
    /// автоопределение)
    #[arg(long = "vpn-interface")]
    vpn_interface: Option<String>,
    /// Порт SOCKS5 на 127.0.0.1 (0 — автоподбор)
    #[arg(long, default_value_t = 0)]
    socks5: u16,
    /// TCP-проброс вида host:port (повторяется; порт слушателя —
    /// автоподбор)
    #[arg(long = "forward")]
    forwards: Vec<String>,
    /// DNS внутри бутылки
    #[arg(long)]
    dns: Option<IpAddr>,
    /// Команда, запускаемая после подъёма бутылки
    #[arg(long)]
    launch: Option<String>,
    /// Перезаписать существующую бутылку этого id
    #[arg(long)]
    force: bool,
}

/// Тип бутылки для флага `--kind` (локальное значение-enum для clap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum KindArg {
    /// Изолирующее окружение (WSL).
    Env,
    /// Фирменный клиент на хосте.
    Host,
}

/// Протокол туннеля для флага `--protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProtocolArg {
    /// WireGuard (userspace wireguard-go).
    Wireguard,
    /// OpenVPN.
    Openvpn,
}

impl Command {
    /// Собирает профиль из флагов `create` (маппинг kind/protocol в
    /// типы ядра; цели пробросов — как есть, валидация на демоне).
    fn create_profile(args: &CreateArgs) -> BottleProfile {
        let vpn = match args.kind {
            KindArg::Env => VpnConfig::Env {
                protocol: match args.protocol {
                    Some(ProtocolArg::Openvpn) => TunnelProtocol::OpenVpn,
                    Some(ProtocolArg::Wireguard) | None => TunnelProtocol::WireGuard,
                },
                config: args
                    .config
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            },
            KindArg::Host => VpnConfig::Host {
                client: args.vpn.clone().unwrap_or_default(),
                args: args.vpn_args.clone(),
                endpoints: args.endpoints.clone(),
                interface: args.vpn_interface.clone(),
            },
        };
        BottleProfile {
            id: args.id.clone(),
            name: args.name.clone(),
            kind: match args.kind {
                KindArg::Env => BottleKind::Env,
                KindArg::Host => BottleKind::Host,
            },
            vpn,
            expose: ExposeConfig {
                socks5: args.socks5,
                forwards: args
                    .forwards
                    .iter()
                    .map(|target| ForwardSpec {
                        listen: 0,
                        target: target.clone(),
                    })
                    .collect(),
                dns: args.dns,
                launch: args.launch.clone(),
            },
        }
    }
}

/// Подкоманды `bottle logs`.
#[derive(Debug, Subcommand)]
enum LogsCommand {
    /// Выгрузить журналы демона и службы в ZIP-архив
    Export {
        /// Куда положить архив (по умолчанию — каталог данных демона)
        #[arg(long)]
        dest: Option<PathBuf>,
    },
}

impl Command {
    /// Преобразует подкоманду в запрос протокола демона.
    // Рендер и маппинг тестируются на любой платформе; вызываются только
    // windows-транспортом (паттерн `dispatch` в daemon/src/pipe.rs).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn to_request(&self) -> CliRequest {
        match self {
            Command::List => CliRequest::List,
            Command::Show { id } => CliRequest::Show { id: id.clone() },
            Command::Start { id } => CliRequest::Start { id: id.clone() },
            Command::Stop { id } => CliRequest::Stop { id: id.clone() },
            Command::Status => CliRequest::Status,
            Command::Doctor => CliRequest::Doctor,
            Command::Create(args) => CliRequest::Create {
                profile: Self::create_profile(args),
                force: args.force,
            },
            Command::Delete { id } => CliRequest::Delete { id: id.clone() },
            Command::Routes {
                command: RoutesCommand::Cleanup,
            } => CliRequest::RoutesCleanup,
            Command::Upgrade => CliRequest::Upgrade,
            Command::Logs {
                command: LogsCommand::Export { dest },
            } => CliRequest::LogsExport { dest: dest.clone() },
        }
    }
}

/// Ожидание ответа демона для быстрых команд (list/show/status/stop).
#[cfg_attr(not(windows), allow(dead_code))]
const REPLY_TIMEOUT: Duration = Duration::from_secs(40);
/// Ожидание ответа демона для `start`: холодный старт складывается из
/// импорта аплаенса в WSL (десятки секунд), подключения агента (~30 с),
/// handshake туннеля (~30 с) и возможного ретрая при занятом порте —
/// худший случай превышает базовые 40 с.
#[cfg_attr(not(windows), allow(dead_code))]
const REPLY_TIMEOUT_START: Duration = Duration::from_secs(120);

/// Окно повторных попыток подключения (named pipe и UDS одинаково):
/// демон обслуживает клиентов параллельно, но между созданием
/// очередного ожидающего экземпляра канала/сокета и освобождением
/// предыдущего клиент видит «занят»/«нет файла» — это стык, а не
/// отсутствие демона; 60 с покрывают его с запасом (включая холодный
/// старт bottled).
#[cfg_attr(not(windows), allow(dead_code))]
const CONNECT_RETRY_WINDOW: Duration = Duration::from_secs(60);

/// Таймаут ответа демона зависит от команды: `start` — долгая операция,
/// остальные укладываются в базовое ожидание.
// Выбирается windows-транспортом; на остальных платформах — только
// тестами (как `Command::to_request`).
#[cfg_attr(not(windows), allow(dead_code))]
fn reply_timeout(request: &CliRequest) -> Duration {
    match request {
        CliRequest::Start { .. } => REPLY_TIMEOUT_START,
        _ => REPLY_TIMEOUT,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Свой флаг --version вместо clap: при сочетании с --json вывод
    // машинный ({"version":"…"}), и работает он без подкоманды.
    if cli.version {
        println!("{}", version_output(cli.json));
        return ExitCode::SUCCESS;
    }

    let Some(command) = cli.command else {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingSubcommand,
                "укажите подкоманду (список — --help)",
            )
            .exit()
    };

    // Windows — named pipe (спека §4.4); macOS/Unix — UDS
    // `<data>/daemon.sock` (M4 Task 7). Рендер и разбор аргументов
    // кроссплатформенны и тестируются на любой платформе.
    #[cfg(windows)]
    {
        let request = command.to_request();
        match client::exchange(&request).await {
            Ok(reply) => {
                if cli.json {
                    handle_reply_json(&reply)
                } else {
                    handle_reply(&command, reply)
                }
            }
            Err(error) => {
                if cli.json {
                    println!(
                        "{}",
                        render_json(&CliReply::Error {
                            message: error.to_string()
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(unix)]
    {
        let request = command.to_request();
        let socket = bottle_core::ipc::daemon_socket_path(
            &bottle_core::ipc::default_data_dir()
                .expect("каталог данных пользователя определимся на unix"),
        );
        match bottle_core::ipc::uds_exchange(
            &socket,
            &request,
            reply_timeout(&request),
            CONNECT_RETRY_WINDOW,
        )
        .await
        {
            Ok(reply) => {
                if cli.json {
                    handle_reply_json(&reply)
                } else {
                    handle_reply(&command, reply)
                }
            }
            Err(message) => {
                if cli.json {
                    println!("{}", render_json(&CliReply::Error { message }));
                } else {
                    eprintln!("{message}");
                }
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = command;
        if cli.json {
            println!(
                "{}",
                render_json(&CliReply::Error {
                    message: "bottle не поддерживает эту платформу (транспорта демона нет)"
                        .to_string()
                })
            );
        } else {
            eprintln!("bottle не поддерживает эту платформу (транспорта демона нет)");
        }
        ExitCode::FAILURE
    }
}

/// Строка `--version`: текст (`bottle 0.2.0`) или JSON для `--json`.
fn version_output(json: bool) -> String {
    if json {
        format!("{{\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("bottle {}", env!("CARGO_PKG_VERSION"))
    }
}

/// Сериализует ответ демона для `--json`: контракт GUI — имена полей
/// CliReply на проводе, без рендера (сериализация этих типов неотказна).
fn render_json(reply: &CliReply) -> String {
    serde_json::to_string(reply).expect("CliReply сериализуется в JSON без потерь")
}

/// Печатает ответ демона в json-режиме: stdout всегда валидный JSON,
/// код выхода повторяет текстовый режим (Error / doctor с fail /
/// show без профиля — 1, прочие успехи — 0).
// Вызывается windows-транспортом; на остальных платформах — только тестами.
#[cfg_attr(not(windows), allow(dead_code))]
fn handle_reply_json(reply: &CliReply) -> ExitCode {
    println!("{}", render_json(reply));
    match reply {
        CliReply::Error { .. } => ExitCode::FAILURE,
        CliReply::DoctorReport { items } => {
            if items
                .iter()
                .any(|item| item.status == bottle_core::ipc::CheckStatus::Fail)
            {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        CliReply::Show { profile: None, .. } => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

/// Печатает ответ демона по команде: успех — рендер в stdout и код 0,
/// ошибка демона — сообщение в stderr и код 1.
// Вызывается windows-транспортом; на остальных платформах — только тестами.
#[cfg_attr(not(windows), allow(dead_code))]
fn handle_reply(command: &Command, reply: CliReply) -> ExitCode {
    match (command, &reply) {
        (Command::List, CliReply::List { bottles }) => {
            println!("{}", render::render_list(bottles));
            ExitCode::SUCCESS
        }
        (Command::Show { id }, CliReply::Show { profile, runtime }) => match profile {
            Some(profile) => {
                println!("{}", render::render_show(profile, runtime.as_ref()));
                ExitCode::SUCCESS
            }
            // Демон отвечает пустым Show на отсутствующий/битый профиль.
            // Рантайм при этом может быть живым (профиль удалили при
            // поднятой бутылке): показываем его до отказа — T13.
            None => {
                if let Some(runtime) = runtime {
                    println!("{}", render::render_show_runtime_only(runtime));
                }
                eprintln!("Профиль бутылки не найден или недоступен: {id}");
                ExitCode::FAILURE
            }
        },
        (Command::Start { .. }, CliReply::Started { id }) => {
            println!("Бутылка {id} поднята");
            ExitCode::SUCCESS
        }
        (Command::Stop { .. }, CliReply::Stopped { id, warning }) => {
            println!("Бутылка {id} опущена");
            // Host-бутылка: VPN-клиент остаётся работать (спека §7.2) —
            // предупреждение демона доходит до пользователя.
            if let Some(warning) = warning {
                eprintln!("Внимание: {warning}");
            }
            ExitCode::SUCCESS
        }
        (Command::Status, CliReply::Status { bottles }) => {
            println!("{}", render::render_status(bottles));
            ExitCode::SUCCESS
        }
        (
            Command::Logs {
                command: LogsCommand::Export { .. },
            },
            CliReply::Path(path),
        ) => {
            println!("Журналы выгружены: {path}");
            println!("{}", render::LOGS_EXPORT_WARNING);
            ExitCode::SUCCESS
        }
        (Command::Create { .. }, CliReply::Created { id }) => {
            println!("Бутылка {id} создана (профиль и конфиг записаны в хранилище)");
            ExitCode::SUCCESS
        }
        (Command::Delete { .. }, CliReply::Deleted { id, warning }) => {
            println!("Бутылка {id} удалена");
            if let Some(warning) = warning {
                eprintln!("Внимание: {warning}");
            }
            ExitCode::SUCCESS
        }
        (Command::Upgrade, CliReply::Upgraded { previous, current }) => {
            match previous {
                Some(previous) => println!(
                    "Аплаенс обновлён: версия {previous} → {current}; \
                     профили бутылок сохранены"
                ),
                None => println!("Аплаенс обновлён: версия {current}; профили бутылок сохранены"),
            }
            ExitCode::SUCCESS
        }
        (Command::Routes { .. }, CliReply::Done { warning }) => {
            println!("Маршруты очищены");
            if let Some(warning) = warning {
                eprintln!("Внимание: {warning}");
            }
            ExitCode::SUCCESS
        }
        (Command::Doctor, CliReply::DoctorReport { items }) => {
            println!("{}", render::render_doctor(items));
            // Отказ — только при fail; warn допустим (portable без
            // службы, аплаенс до первого старта — не поломка).
            if items
                .iter()
                .any(|item| item.status == bottle_core::ipc::CheckStatus::Fail)
            {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        (_, CliReply::Error { message }) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        _ => {
            eprintln!("Неожиданный ответ демона: {reply:?}");
            ExitCode::FAILURE
        }
    }
}

/// Транспорт демона: подключение к `\\.\pipe\bottled`, кадры
/// [`LengthJsonCodec`](bottle_core::frame::LengthJsonCodec), один
/// запрос — один ответ на соединение.
#[cfg(windows)]
mod client {
    use std::io;
    use std::time::Duration;

    use super::reply_timeout;
    use bottle_core::frame::{FrameError, LengthJsonCodec};
    use bottle_core::ipc::{CliReply, CliRequest, DAEMON_PIPE};
    use bytes::BytesMut;
    use tokio::io::{split, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Encoder, FramedRead};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

    /// Пауза между попытками подключения.
    const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(50);

    /// Ошибки обмена с демоном; тексты — пользовательские, на русском.
    #[derive(Debug, thiserror::Error)]
    pub enum ClientError {
        /// Канал не стал доступен за окно повторов: демон не запущен.
        #[error("Не удалось подключиться к демону BottleForApps — запущен ли bottled?")]
        DaemonUnavailable,
        /// Демон не прислал ответ за отведённое время.
        #[error("Демон не ответил за {} с", .0.as_secs())]
        Timeout(Duration),
        /// Демон закрыл соединение, не ответив.
        #[error("Демон закрыл соединение без ответа")]
        Closed,
        /// Ошибка ввода-вывода канала.
        #[error("Ошибка обмена с демоном: {0}")]
        Io(#[from] io::Error),
        /// Ответ не разобран как кадр протокола.
        #[error("Ошибка обмена с демоном: {0}")]
        Frame(#[from] FrameError),
    }

    /// Обмен «запрос → ответ» с демоном по named pipe.
    pub async fn exchange(request: &CliRequest) -> Result<CliReply, ClientError> {
        let pipe = connect().await?;
        let (read_half, mut write_half) = split(pipe);
        let mut reader = FramedRead::new(read_half, LengthJsonCodec::<CliReply>::new());
        send_request(&mut write_half, request).await?;

        match tokio::time::timeout(reply_timeout(request), reader.next()).await {
            Ok(Some(Ok(reply))) => Ok(reply),
            Ok(Some(Err(error))) => Err(ClientError::Frame(error)),
            Ok(None) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout(reply_timeout(request))),
        }
    }

    /// Подключается к каналу демона с повторами: `ERROR_PIPE_BUSY` и
    /// `ERROR_FILE_NOT_FOUND` в окне [`CONNECT_RETRY_WINDOW`] — стык
    /// между экземплярами канала сервера, а не отсутствие демона.
    async fn connect() -> Result<NamedPipeClient, ClientError> {
        let deadline = tokio::time::Instant::now() + super::CONNECT_RETRY_WINDOW;
        loop {
            match ClientOptions::new().open(DAEMON_PIPE) {
                Ok(pipe) => return Ok(pipe),
                Err(error) if busy_or_missing(&error) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ClientError::DaemonUnavailable);
                    }
                    tokio::time::sleep(CONNECT_RETRY_PAUSE).await;
                }
                Err(error) => return Err(ClientError::Io(error)),
            }
        }
    }

    /// Код ошибки Win32 — «канал занят» или «канал ещё не создан».
    fn busy_or_missing(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(code) if code == ERROR_PIPE_BUSY as i32
            || code == ERROR_FILE_NOT_FOUND as i32)
    }

    /// Кодирует и отправляет запрос (fallible-путь `Encoder`, как в демоне).
    async fn send_request<W>(write_half: &mut W, request: &CliRequest) -> Result<(), ClientError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut codec = LengthJsonCodec::<CliRequest>::new();
        let mut buf = BytesMut::new();
        codec
            .encode(request.clone(), &mut buf)
            .map_err(io::Error::other)?;
        write_half.write_all(&buf).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args)
            .expect("аргументы корректны")
            .command
            .expect("подкоманда задана")
    }

    #[test]
    fn parses_every_subcommand() {
        assert!(matches!(parse(&["bottle", "list"]), Command::List));
        assert!(matches!(parse(&["bottle", "status"]), Command::Status));
        assert!(matches!(parse(&["bottle", "doctor"]), Command::Doctor));
        assert!(matches!(
            parse(&["bottle", "show", "office-a"]),
            Command::Show { id } if id == "office-a"
        ));
        assert!(matches!(
            parse(&["bottle", "start", "office-a"]),
            Command::Start { id } if id == "office-a"
        ));
        assert!(matches!(
            parse(&["bottle", "stop", "office-a"]),
            Command::Stop { id } if id == "office-a"
        ));
    }

    /// Полный набор флагов create для env-бутылки: parse → to_request
    /// собирает корректный профиль.
    #[test]
    fn create_env_maps_flags_to_profile() {
        let command = parse(&[
            "bottle",
            "create",
            "office-a",
            "--name",
            "Офис А",
            "--kind",
            "env",
            "--protocol",
            "wireguard",
            "--config",
            r"C:\wg\office.conf",
            "--socks5",
            "1080",
            "--forward",
            "10.20.30.15:3389",
            "--dns",
            "10.20.30.1",
            "--launch",
            "mstsc",
        ]);
        assert_eq!(
            command.to_request(),
            CliRequest::Create {
                profile: bottle_core::profile::BottleProfile {
                    id: "office-a".to_string(),
                    name: "Офис А".to_string(),
                    kind: bottle_core::profile::BottleKind::Env,
                    vpn: bottle_core::profile::VpnConfig::Env {
                        protocol: bottle_core::profile::TunnelProtocol::WireGuard,
                        config: r"C:\wg\office.conf".to_string(),
                    },
                    expose: bottle_core::profile::ExposeConfig {
                        socks5: 1080,
                        forwards: vec![bottle_core::profile::ForwardSpec {
                            listen: 0,
                            target: "10.20.30.15:3389".to_string(),
                        }],
                        dns: Some("10.20.30.1".parse().unwrap()),
                        launch: Some("mstsc".to_string()),
                    },
                },
                force: false,
            }
        );
    }

    /// host-бутылка: --vpn/--vpn-args/--endpoint/--vpn-interface.
    #[test]
    fn create_host_maps_flags_to_profile() {
        let command = parse(&[
            "bottle",
            "create",
            "corp",
            "--name",
            "Корп",
            "--kind",
            "host",
            "--vpn",
            r"C:\vpn.exe",
            "--vpn-args",
            "connect office",
            "--endpoint",
            "vpn1.example.com",
            "--endpoint",
            "vpn2.example.com",
            "--vpn-interface",
            "Vendor Adapter",
        ]);
        assert_eq!(
            command.to_request(),
            CliRequest::Create {
                profile: bottle_core::profile::BottleProfile {
                    id: "corp".to_string(),
                    name: "Корп".to_string(),
                    kind: bottle_core::profile::BottleKind::Host,
                    vpn: bottle_core::profile::VpnConfig::Host {
                        client: r"C:\vpn.exe".to_string(),
                        args: Some("connect office".to_string()),
                        endpoints: vec![
                            "vpn1.example.com".to_string(),
                            "vpn2.example.com".to_string(),
                        ],
                        interface: Some("Vendor Adapter".to_string()),
                    },
                    expose: bottle_core::profile::ExposeConfig {
                        socks5: 0,
                        forwards: vec![],
                        dns: None,
                        launch: None,
                    },
                },
                force: false,
            }
        );
    }

    #[test]
    fn parses_create_force_and_delete() {
        assert!(matches!(
            parse(&[
                "bottle", "create", "a", "--name", "N", "--kind", "host",
                "--vpn", "v.exe", "--endpoint", "e.example", "--force"
            ]),
            Command::Create(args) if args.force
        ));
        assert_eq!(
            parse(&["bottle", "delete", "office-a"]).to_request(),
            CliRequest::Delete {
                id: "office-a".to_string()
            }
        );
    }

    /// upgrade и routes cleanup разбираются и отображаются в запросы.
    #[test]
    fn parses_upgrade_and_routes_cleanup() {
        assert!(matches!(parse(&["bottle", "upgrade"]), Command::Upgrade));
        assert_eq!(
            parse(&["bottle", "upgrade"]).to_request(),
            CliRequest::Upgrade
        );
        assert_eq!(
            parse(&["bottle", "routes", "cleanup"]).to_request(),
            CliRequest::RoutesCleanup
        );
    }

    /// Ответы Upgraded/Done: успех; предупреждение cleanup — на stderr.
    #[test]
    fn upgraded_done_replies_are_success() {
        assert_eq!(
            handle_reply(
                &Command::Upgrade,
                CliReply::Upgraded {
                    previous: Some("0.1.0".to_string()),
                    current: "0.2.0".to_string(),
                },
            ),
            ExitCode::SUCCESS
        );
        assert_eq!(
            handle_reply(
                &Command::Routes {
                    command: RoutesCommand::Cleanup
                },
                CliReply::Done {
                    warning: Some("служба маршрутов не установлена".to_string()),
                },
            ),
            ExitCode::SUCCESS
        );
    }

    /// Ответы Created/Deleted: успех; warning демона доходит (код 0).
    #[test]
    fn created_deleted_replies_are_success() {
        assert_eq!(
            handle_reply(
                &parse(&[
                    "bottle",
                    "create",
                    "a",
                    "--name",
                    "N",
                    "--kind",
                    "host",
                    "--vpn",
                    "v.exe",
                    "--endpoint",
                    "e.example"
                ]),
                CliReply::Created {
                    id: "a".to_string()
                },
            ),
            ExitCode::SUCCESS
        );
        assert_eq!(
            handle_reply(
                &parse(&["bottle", "delete", "a"]),
                CliReply::Deleted {
                    id: "a".to_string(),
                    warning: Some("файл конфигурации отсутствовал".to_string()),
                },
            ),
            ExitCode::SUCCESS
        );
    }

    /// Doctor: exit 0 без fail (warn допустим), fail — exit 1.
    #[test]
    fn doctor_exit_code_follows_worst_status() {
        use bottle_core::ipc::{CheckItem, CheckStatus};
        let items = |status| {
            vec![
                CheckItem {
                    code: "wsl".to_string(),
                    status: CheckStatus::Ok,
                    detail: "WSL включён".to_string(),
                },
                CheckItem {
                    code: "service".to_string(),
                    status,
                    detail: "…".to_string(),
                },
            ]
        };
        assert_eq!(
            handle_reply(
                &Command::Doctor,
                CliReply::DoctorReport {
                    items: items(CheckStatus::Warn)
                }
            ),
            ExitCode::SUCCESS,
            "warn — не отказ"
        );
        assert_eq!(
            handle_reply(
                &Command::Doctor,
                CliReply::DoctorReport {
                    items: items(CheckStatus::Fail)
                }
            ),
            ExitCode::FAILURE,
            "fail — отказ"
        );
    }

    /// Таблица doctor содержит эмодзи-статусы всех трёх видов.
    #[test]
    fn doctor_render_has_status_glyphs() {
        use bottle_core::ipc::{CheckItem, CheckStatus};
        let items = vec![
            CheckItem {
                code: "wsl".to_string(),
                status: CheckStatus::Ok,
                detail: "WSL включён".to_string(),
            },
            CheckItem {
                code: "service".to_string(),
                status: CheckStatus::Warn,
                detail: "не установлена".to_string(),
            },
            CheckItem {
                code: "agent".to_string(),
                status: CheckStatus::Fail,
                detail: "не отвечает".to_string(),
            },
        ];
        let table = render::render_doctor(&items);
        assert!(table.contains('✓'), "ok: {table}");
        assert!(table.contains('⚠'), "warn: {table}");
        assert!(table.contains('✗'), "fail: {table}");
        assert!(table.contains("wsl"), "код проверки: {table}");
        assert!(table.contains("не отвечает"), "пояснение: {table}");
    }

    #[test]
    fn parses_logs_export_with_optional_dest() {
        assert!(matches!(
            parse(&["bottle", "logs", "export"]),
            Command::Logs {
                command: LogsCommand::Export { dest: None }
            }
        ));
        assert!(matches!(
            parse(&["bottle", "logs", "export", "--dest", r"C:\tmp\logs.zip"]),
            Command::Logs {
                command: LogsCommand::Export {
                    dest: Some(path)
                }
            } if path == std::path::Path::new(r"C:\tmp\logs.zip")
        ));
    }

    #[test]
    fn logs_export_maps_to_protocol_request() {
        assert_eq!(
            Command::Logs {
                command: LogsCommand::Export {
                    dest: Some(std::path::PathBuf::from("/tmp/logs.zip"))
                }
            }
            .to_request(),
            CliRequest::LogsExport {
                dest: Some(std::path::PathBuf::from("/tmp/logs.zip"))
            }
        );
        assert_eq!(
            Command::Logs {
                command: LogsCommand::Export { dest: None }
            }
            .to_request(),
            CliRequest::LogsExport { dest: None }
        );
    }

    /// Ответ Path печатается успехом; предупреждение о содержимом
    /// (спека §10) упоминает «просмотрите».
    #[test]
    fn logs_export_reply_is_success_and_warns_before_sending() {
        let command = Command::Logs {
            command: LogsCommand::Export { dest: None },
        };
        assert_eq!(
            handle_reply(
                &command,
                CliReply::Path(r"C:\tmp\bottleforapps-logs-20261004-1200.zip".to_string())
            ),
            ExitCode::SUCCESS
        );
        assert!(
            render::LOGS_EXPORT_WARNING.contains("просмотрите"),
            "предупреждение о содержимом: {}",
            render::LOGS_EXPORT_WARNING
        );
    }

    #[test]
    fn id_is_required_for_show_start_stop() {
        for command in ["show", "start", "stop"] {
            assert!(
                Cli::try_parse_from(["bottle", command]).is_err(),
                "«{command}» без id должен отказывать"
            );
        }
    }

    // ---- `--json`-режим (M4 Task 1): контракт для GUI ----

    /// Флаг `--json` глобальный: принимается до и после подкоманды.
    #[test]
    fn json_flag_parses_before_and_after_subcommand() {
        for args in [
            vec!["bottle", "--json", "list"],
            vec!["bottle", "list", "--json"],
            vec![
                "bottle",
                "create",
                "a",
                "--name",
                "N",
                "--kind",
                "host",
                "--vpn",
                "v.exe",
                "--endpoint",
                "e.example",
                "--json",
            ],
        ] {
            let cli = Cli::try_parse_from(args.clone()).expect("--json в любой позиции");
            assert!(cli.json, "{args:?}");
        }
    }

    /// `--version` работает без подкоманды (свой флаг вместо clap).
    #[test]
    fn version_flag_parses_without_subcommand() {
        let cli = Cli::try_parse_from(["bottle", "--version"]).expect("--version без подкоманды");
        assert!(cli.version);
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["bottle", "--version", "--json"]).expect("с --json");
        assert!(cli.version && cli.json);
    }

    /// `--version --json` → `{"version":"…"}`; текстовый формат не меняется.
    #[test]
    fn version_output_text_and_json_shapes() {
        assert_eq!(
            version_output(false),
            format!("bottle {}", env!("CARGO_PKG_VERSION"))
        );
        let json = version_output(true);
        assert_eq!(
            json,
            format!("{{\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION"))
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("корректный JSON");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    fn json_list_reply() -> CliReply {
        use bottle_core::ipc::BottleSummary;
        CliReply::List {
            bottles: vec![
                BottleSummary {
                    id: "office-a".to_string(),
                    name: "Офис А".to_string(),
                    kind: bottle_core::profile::BottleKind::Env,
                    state: Some("up".to_string()),
                },
                BottleSummary {
                    id: "corp".to_string(),
                    name: "Корп VPN".to_string(),
                    kind: bottle_core::profile::BottleKind::Host,
                    state: None,
                },
            ],
        }
    }

    /// Golden: list — стабильные имена полей CliReply.
    #[test]
    fn json_golden_list() {
        assert_eq!(
            render_json(&json_list_reply()),
            r#"{"List":{"bottles":[{"id":"office-a","name":"Офис А","kind":"env","state":"up"},{"id":"corp","name":"Корп VPN","kind":"host","state":null}]}}"#
        );
    }

    /// Golden: show с профилем и рантаймом.
    #[test]
    fn json_golden_show_full() {
        let reply = CliReply::Show {
            profile: Some(bottle_core::profile::BottleProfile {
                id: "office-a".to_string(),
                name: "Офис А".to_string(),
                kind: bottle_core::profile::BottleKind::Env,
                vpn: bottle_core::profile::VpnConfig::Env {
                    protocol: bottle_core::profile::TunnelProtocol::WireGuard,
                    config: "wg-office-a.conf".to_string(),
                },
                expose: bottle_core::profile::ExposeConfig {
                    socks5: 1080,
                    forwards: vec![bottle_core::profile::ForwardSpec {
                        listen: 33389,
                        target: "10.20.30.15:3389".to_string(),
                    }],
                    dns: Some("10.20.30.1".parse().unwrap()),
                    launch: Some("mstsc".to_string()),
                },
            }),
            runtime: Some(bottle_core::ipc::RuntimeState {
                id: "office-a".to_string(),
                state: bottle_core::ipc::BottleState::Up,
                socks5: Some(1080),
                forwards: vec![bottle_core::protocol::ForwardPort {
                    port: 33389,
                    target: "10.20.30.15:3389".to_string(),
                }],
            }),
        };
        assert_eq!(
            render_json(&reply),
            r#"{"Show":{"profile":{"id":"office-a","name":"Офис А","kind":"env","vpn":{"protocol":"wireguard","config":"wg-office-a.conf"},"expose":{"socks5":1080,"forwards":[{"listen":33389,"target":"10.20.30.15:3389"}],"dns":"10.20.30.1","launch":"mstsc"}},"runtime":{"id":"office-a","state":"Up","socks5":1080,"forwards":[{"port":33389,"target":"10.20.30.15:3389"}]}}}"#
        );
    }

    /// Golden: show без профиля — null-половки, не ошибка формата.
    #[test]
    fn json_golden_show_missing_profile() {
        let reply = CliReply::Show {
            profile: None,
            runtime: None,
        };
        assert_eq!(
            render_json(&reply),
            r#"{"Show":{"profile":null,"runtime":null}}"#
        );
    }

    /// Golden: остальные варианты ответов одной строкой каждый.
    #[test]
    fn json_golden_single_variant_replies() {
        assert_eq!(
            render_json(&CliReply::Started {
                id: "office-a".to_string()
            }),
            r#"{"Started":{"id":"office-a"}}"#
        );
        assert_eq!(
            render_json(&CliReply::Stopped {
                id: "office-a".to_string(),
                warning: None
            }),
            r#"{"Stopped":{"id":"office-a","warning":null}}"#
        );
        assert_eq!(
            render_json(&CliReply::Stopped {
                id: "corp".to_string(),
                warning: Some("VPN-клиент оставлен запущенным".to_string())
            }),
            r#"{"Stopped":{"id":"corp","warning":"VPN-клиент оставлен запущенным"}}"#
        );
        assert_eq!(
            render_json(&CliReply::Path(
                r"C:\tmp\bottleforapps-logs-20261004-1200.zip".to_string()
            )),
            r#"{"Path":"C:\\tmp\\bottleforapps-logs-20261004-1200.zip"}"#
        );
        assert_eq!(
            render_json(&CliReply::Created {
                id: "office-a".to_string()
            }),
            r#"{"Created":{"id":"office-a"}}"#
        );
        assert_eq!(
            render_json(&CliReply::Deleted {
                id: "office-a".to_string(),
                warning: None
            }),
            r#"{"Deleted":{"id":"office-a","warning":null}}"#
        );
        assert_eq!(
            render_json(&CliReply::Upgraded {
                previous: Some("0.1.0".to_string()),
                current: "0.2.0".to_string()
            }),
            r#"{"Upgraded":{"previous":"0.1.0","current":"0.2.0"}}"#
        );
        assert_eq!(
            render_json(&CliReply::Done { warning: None }),
            r#"{"Done":{"warning":null}}"#
        );
        assert_eq!(
            render_json(&CliReply::Error {
                message: "бутылка office-a не найдена".to_string()
            }),
            r#"{"Error":{"message":"бутылка office-a не найдена"}}"#
        );
    }

    /// Golden: status с состояниями Up/Degraded (варианты BottleState —
    /// внешний тег CamelCase, как на проводе протокола).
    #[test]
    fn json_golden_status() {
        let reply = CliReply::Status {
            bottles: vec![
                bottle_core::ipc::RuntimeState {
                    id: "office-a".to_string(),
                    state: bottle_core::ipc::BottleState::Up,
                    socks5: Some(1080),
                    forwards: vec![],
                },
                bottle_core::ipc::RuntimeState {
                    id: "office-b".to_string(),
                    state: bottle_core::ipc::BottleState::Degraded {
                        reason: "агент аплаенса не отвечает".to_string(),
                    },
                    socks5: None,
                    forwards: vec![],
                },
            ],
        };
        assert_eq!(
            render_json(&reply),
            r#"{"Status":{"bottles":[{"id":"office-a","state":"Up","socks5":1080,"forwards":[]},{"id":"office-b","state":{"Degraded":{"reason":"агент аплаенса не отвечает"}},"socks5":null,"forwards":[]}]}}"#
        );
    }

    /// Golden: doctor-отчёт со всеми тремя статусами проверок.
    #[test]
    fn json_golden_doctor_report() {
        use bottle_core::ipc::{CheckItem, CheckStatus};
        let reply = CliReply::DoctorReport {
            items: vec![
                CheckItem {
                    code: "wsl".to_string(),
                    status: CheckStatus::Ok,
                    detail: "WSL включён, дистрибуций: 2".to_string(),
                },
                CheckItem {
                    code: "service".to_string(),
                    status: CheckStatus::Warn,
                    detail: "Служба маршрутов не установлена".to_string(),
                },
                CheckItem {
                    code: "agent".to_string(),
                    status: CheckStatus::Fail,
                    detail: "агент не отвечает".to_string(),
                },
            ],
        };
        assert_eq!(
            render_json(&reply),
            r#"{"DoctorReport":{"items":[{"code":"wsl","status":"Ok","detail":"WSL включён, дистрибуций: 2"},{"code":"service","status":"Warn","detail":"Служба маршрутов не установлена"},{"code":"agent","status":"Fail","detail":"агент не отвечает"}]}}"#
        );
    }

    /// Контракт секретов: профиль несёт ИМЯ конфига, не содержимое —
    /// маркеры WG/OpenVPN-конфига не встречаются ни в одном json-выводе
    /// (Review Focus #1).
    #[test]
    fn json_never_contains_config_contents() {
        let profile = bottle_core::profile::BottleProfile {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: bottle_core::profile::BottleKind::Env,
            vpn: bottle_core::profile::VpnConfig::Env {
                protocol: bottle_core::profile::TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
            },
            expose: bottle_core::profile::ExposeConfig {
                socks5: 1080,
                forwards: vec![],
                dns: None,
                launch: None,
            },
        };
        let show = CliReply::Show {
            profile: Some(profile),
            runtime: None,
        };
        let json = render_json(&show);
        // Поле config — точно имя файла, не содержимое.
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("show-ответ — корректный JSON");
        assert_eq!(
            value["Show"]["profile"]["vpn"]["config"],
            "wg-office-a.conf"
        );
        // Маркеры содержимого WG/OpenVPN-конфига отсутствуют.
        for marker in ["[Interface]", "[Peer]", "PrivateKey", "<ca>", "<tls-auth>"] {
            assert!(!json.contains(marker), "секрет {marker:?} утёк в {json}");
        }
        // И в list-выводе их нет.
        let list_json = render_json(&json_list_reply());
        for marker in ["[Interface]", "PrivateKey"] {
            assert!(!list_json.contains(marker));
        }
    }

    /// Коды выхода json-режима повторяют текстовый: Error — 1, doctor
    /// с fail — 1 (отчёт всё равно напечатан), show без профиля — 1,
    /// остальные успехи — 0.
    #[test]
    fn json_exit_codes_mirror_text_mode() {
        use bottle_core::ipc::{CheckItem, CheckStatus};
        assert_eq!(
            handle_reply_json(&CliReply::Error {
                message: "x".to_string()
            }),
            ExitCode::FAILURE
        );
        assert_eq!(
            handle_reply_json(&CliReply::DoctorReport {
                items: vec![CheckItem {
                    code: "agent".to_string(),
                    status: CheckStatus::Fail,
                    detail: "…".to_string()
                }]
            }),
            ExitCode::FAILURE
        );
        assert_eq!(
            handle_reply_json(&CliReply::DoctorReport {
                items: vec![CheckItem {
                    code: "service".to_string(),
                    status: CheckStatus::Warn,
                    detail: "…".to_string()
                }]
            }),
            ExitCode::SUCCESS
        );
        assert_eq!(
            handle_reply_json(&CliReply::Show {
                profile: None,
                runtime: None
            }),
            ExitCode::FAILURE
        );
        for reply in [
            CliReply::Started {
                id: "a".to_string(),
            },
            CliReply::Created {
                id: "a".to_string(),
            },
            CliReply::Done { warning: None },
            CliReply::Path("p.zip".to_string()),
            json_list_reply(),
        ] {
            assert_eq!(handle_reply_json(&reply), ExitCode::SUCCESS, "{reply:?}");
        }
    }

    #[test]
    fn reply_timeout_is_per_command() {
        // `start` — длинная операция: холодный старт (импорт аплаенса,
        // агент, handshake) не укладывается в базовое ожидание.
        assert_eq!(
            reply_timeout(&CliRequest::Start {
                id: "office-a".to_string()
            }),
            Duration::from_secs(120)
        );
        for request in [
            CliRequest::List,
            CliRequest::Status,
            CliRequest::Show {
                id: "office-a".to_string(),
            },
            CliRequest::Stop {
                id: "office-a".to_string(),
            },
        ] {
            assert_eq!(reply_timeout(&request), Duration::from_secs(40));
        }
    }

    #[test]
    fn subcommands_map_to_protocol_requests() {
        assert_eq!(Command::List.to_request(), CliRequest::List);
        assert_eq!(Command::Status.to_request(), CliRequest::Status);
        assert_eq!(
            Command::Show {
                id: "office-a".to_string()
            }
            .to_request(),
            CliRequest::Show {
                id: "office-a".to_string()
            }
        );
        assert_eq!(
            Command::Start {
                id: "office-a".to_string()
            }
            .to_request(),
            CliRequest::Start {
                id: "office-a".to_string()
            }
        );
        assert_eq!(
            Command::Stop {
                id: "office-a".to_string()
            }
            .to_request(),
            CliRequest::Stop {
                id: "office-a".to_string()
            }
        );
    }

    /// T13: Show без профиля — отказ (exit 1) и при живом рантайме:
    /// рендер блока проверяет `render::render_show_runtime_only`,
    /// здесь — только код возврата.
    #[test]
    fn show_missing_profile_fails_even_with_runtime() {
        let runtime = bottle_core::ipc::RuntimeState {
            id: "office-a".to_string(),
            state: bottle_core::ipc::BottleState::Up,
            socks5: Some(55001),
            forwards: Vec::new(),
        };
        let command = Command::Show {
            id: "office-a".to_string(),
        };
        assert_eq!(
            handle_reply(
                &command,
                CliReply::Show {
                    profile: None,
                    runtime: Some(runtime),
                }
            ),
            ExitCode::FAILURE,
            "недоступный профиль — отказ, даже если бутылка жива"
        );
        // Базовый случай: ни профиля, ни рантайма — тоже отказ.
        assert_eq!(
            handle_reply(
                &command,
                CliReply::Show {
                    profile: None,
                    runtime: None,
                }
            ),
            ExitCode::FAILURE
        );
    }
}
