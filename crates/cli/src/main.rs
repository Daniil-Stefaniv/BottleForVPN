//! `bottle` — служебный CLI демона «Погребок» (0.3, спека §4.4).
//!
//! Управление бутылками переехало в приложение «Погребок» (GUI — источник
//! истины); CLI оставлен для служебных команд: `doctor`, `logs export`,
//! `upgrade`, `routes cleanup`, `daemon stop`. Legacy-команды
//! (list/show/create/delete/start/stop/status) разбираются ради понятной
//! навигации: CLI печатает «надгробие» с указанием на приложение и
//! завершается кодом 2, не касаясь демона.
//!
//! Флаг `--json` переключает вывод в машинный режим: stdout несёт
//! сериализованный [`CliReply`], ошибки — `{"Error":…}`; этим режимом
//! пользуется GUI как сайдкар-подпроцесс (`upgrade`).

mod render;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bottle_core::ipc::{CliReply, CliRequest};
use clap::{CommandFactory, Parser, Subcommand};

/// Код выхода надгробия: как у ошибки разбора аргументов (2), чтобы
/// скрипты отличали «команды больше нет» от отказа демона (1).
const TOMBSTONE_EXIT_CODE: u8 = 2;

/// Навигация для удалённых команд (0.3): управление бутылками — в GUI.
const TOMBSTONE_MESSAGE: &str = "управление бутылками переехало в приложение «Погребок» — \
     откройте его и работайте там; команды list/show/create/delete/start/stop/status \
     в CLI больше не поддерживаются";

#[derive(Debug, Parser)]
#[command(
    name = "pogrebok",
    about = "Погребок — служебные команды демона (управление бутылками — в приложении)",
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

#[derive(Debug, Subcommand)]
enum Command {
    /// Диагностика окружения (WSL, аплаенс, служба, порты)
    Doctor,
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
    Upgrade {
        /// Полный сброс: остановить все бутылки, погасить WSL целиком
        /// и переустановить дистрибуцию (когда обычный upgrade не помог)
        #[arg(long)]
        force: bool,
    },
    /// Управление демоном
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    // --- Надгробия 0.3: разбираются (со старыми флагами), но демону
    // не отправляются — пользователь получает навигацию в приложение.
    /// Удалена в 0.3: список бутылок — в приложении «Погребок»
    List {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: профиль бутылки — в приложении «Погребок»
    Show {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: создание бутылки — в приложении «Погребок»
    Create {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: удаление бутылки — в приложении «Погребок»
    Delete {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: запуск бутылки — в приложении «Погребок»
    Start {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: остановка бутылки — в приложении «Погребок»
    Stop {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Удалена в 0.3: состояние бутылок — в приложении «Погребок»
    Status {
        /// Аргументы legacy-формы (игнорируются)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Подкоманды `bottle daemon`.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Остановить все бутылки и завершить процесс демона
    Stop,
}

/// Подкоманды `bottle routes`.
#[derive(Debug, Subcommand)]
enum RoutesCommand {
    /// Снять сиротские маршруты через службу pogrebok-svc
    Cleanup,
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
    /// true — команда удалена в 0.3 (управление бутылками): демону не
    /// отправляется, пользователь получает [`TOMBSTONE_MESSAGE`].
    fn is_tombstone(&self) -> bool {
        matches!(
            self,
            Command::List { .. }
                | Command::Show { .. }
                | Command::Create { .. }
                | Command::Delete { .. }
                | Command::Start { .. }
                | Command::Stop { .. }
                | Command::Status { .. }
        )
    }

    /// Преобразует служебную подкоманду в запрос протокола демона.
    // Рендер и маппинг тестируются на любой платформе; вызываются только
    // транспортом (паттерн `dispatch` в daemon/src/pipe.rs).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn to_request(&self) -> CliRequest {
        match self {
            Command::Doctor => CliRequest::Doctor,
            Command::Routes {
                command: RoutesCommand::Cleanup,
            } => CliRequest::RoutesCleanup,
            Command::Upgrade { force } => CliRequest::Upgrade { force: *force },
            Command::Daemon {
                command: DaemonCommand::Stop,
            } => CliRequest::ShutdownDaemon,
            Command::Logs {
                command: LogsCommand::Export { dest },
            } => CliRequest::LogsExport { dest: dest.clone() },
            tombstone => unreachable!("надгробие не уходит демону: {tombstone:?}"),
        }
    }
}

/// Что напечатать на надгробие: текстовый режим — навигация в stderr,
/// json — `{"Error":…}` в stdout (контракт машинного режима сохранён).
fn tombstone_output(json: bool) -> String {
    if json {
        render_json(&CliReply::Error {
            message: TOMBSTONE_MESSAGE.to_string(),
        })
    } else {
        TOMBSTONE_MESSAGE.to_string()
    }
}

/// Ожидание ответа демона для служебных команд.
#[cfg_attr(not(windows), allow(dead_code))]
const REPLY_TIMEOUT: Duration = Duration::from_secs(40);

/// Окно повторных попыток подключения (named pipe и UDS одинаково):
/// демон обслуживает клиентов параллельно, но между созданием
/// очередного ожидающего экземпляра канала/сокета и освобождением
/// предыдущего клиент видит «занят»/«нет файла» — это стык, а не
/// отсутствие демона; 60 с покрывают его с запасом (включая холодный
/// старт pogrebok-daemon).
#[cfg_attr(not(windows), allow(dead_code))]
const CONNECT_RETRY_WINDOW: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> ExitCode {
    // Разбор с json-осознанными ошибками: код 2 остаётся за clap,
    // но в json-режиме сообщение уходит JSON-строкой в stdout.
    let argv: Vec<String> = std::env::args().collect();
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if let Some(json) = format_parse_error(&argv_refs, &error) {
                println!("{json}");
                return ExitCode::from(2);
            }
            error.exit();
        }
    };

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

    // Надгробие (0.3): legacy-команда управления бутылками — навигация
    // в приложение и код 2, демон не трогаем.
    if command.is_tombstone() {
        if cli.json {
            println!("{}", tombstone_output(true));
        } else {
            eprintln!("{}", tombstone_output(false));
        }
        return ExitCode::from(TOMBSTONE_EXIT_CODE);
    }

    // Windows — named pipe (спека §4.4); macOS/Unix — UDS
    // `<data>/daemon.sock` (M4 Task 7). Рендер и разбор аргументов
    // кроссплатформенны и тестируются на любой платформе.
    #[cfg(any(windows, unix))]
    {
        match daemon_exchange(&command.to_request()).await {
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
                    message: "pogrebok не поддерживает эту платформу (транспорта демона нет)"
                        .to_string()
                })
            );
        } else {
            eprintln!("pogrebok не поддерживает эту платформу (транспорта демона нет)");
        }
        ExitCode::FAILURE
    }
}

/// Обмен с демоном по транспорту платформы: Windows — named pipe,
/// macOS/Unix — UDS `<data>/daemon.sock`. Ошибки — русские строки.
#[cfg(windows)]
async fn daemon_exchange(request: &CliRequest) -> Result<CliReply, String> {
    client::exchange(request).await.map_err(|e| e.to_string())
}

/// Обмен с демоном по транспорту платформы: Windows — named pipe,
/// macOS/Unix — UDS `<data>/daemon.sock`. Ошибки — русские строки.
#[cfg(unix)]
async fn daemon_exchange(request: &CliRequest) -> Result<CliReply, String> {
    let socket = bottle_core::ipc::daemon_socket_path(
        &bottle_core::ipc::default_data_dir()
            .expect("каталог данных пользователя определимся на unix"),
    );
    bottle_core::ipc::uds_exchange(&socket, request, REPLY_TIMEOUT, CONNECT_RETRY_WINDOW).await
}

/// Ошибка разбора аргументов в json-режиме: `{"Error":…}` в stdout
/// (код 2). `None` — обычный режим: clap печатает сам (I5 ревью).
fn format_parse_error(argv: &[&str], error: &clap::Error) -> Option<String> {
    if !argv.contains(&"--json") {
        return None;
    }
    // Краткое сообщение (первая строка) — usage-обвязка машинному
    // клиенту не нужна, она остаётся в текстовом режиме.
    let brief = error
        .to_string()
        .lines()
        .next()
        .unwrap_or("ошибка разбора аргументов")
        .trim_start_matches("error: ")
        .to_string();
    Some(render_json(&CliReply::Error { message: brief }))
}

/// Строка `--version`: текст (`pogrebok 0.3.0`) или JSON для `--json`.
fn version_output(json: bool) -> String {
    if json {
        format!("{{\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("pogrebok {}", env!("CARGO_PKG_VERSION"))
    }
}

/// Сериализует ответ демона для `--json`: контракт GUI — имена полей
/// CliReply на проводе, без рендера (сериализация этих типов неотказна).
fn render_json(reply: &CliReply) -> String {
    serde_json::to_string(reply).expect("CliReply сериализуется в JSON без потерь")
}

/// Печатает ответ демона в json-режиме: stdout всегда валидный JSON,
/// код выхода повторяет текстовый режим (Error / doctor с fail — 1,
/// прочие успехи — 0).
// Вызывается транспортом; на остальных платформах — только тестами.
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
        _ => ExitCode::SUCCESS,
    }
}

/// Печатает ответ демона по команде: успех — рендер в stdout и код 0,
/// ошибка демона — сообщение в stderr и код 1.
// Вызывается транспортом; на остальных платформах — только тестами.
#[cfg_attr(not(windows), allow(dead_code))]
fn handle_reply(command: &Command, reply: CliReply) -> ExitCode {
    match (command, &reply) {
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
        (Command::Upgrade { .. }, CliReply::Upgraded { previous, current }) => {
            match previous {
                Some(previous) => println!(
                    "Аплаенс обновлён: версия {previous} → {current}; \
                     профили бутылок сохранены"
                ),
                None => println!("Аплаенс обновлён: версия {current}; профили бутылок сохранены"),
            }
            ExitCode::SUCCESS
        }
        (Command::Daemon { .. }, CliReply::Done { warning }) => {
            println!("Демон останавливается (бутылки остановлены)");
            if let Some(warning) = warning {
                eprintln!("Внимание: {warning}");
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

/// Транспорт демона: подключение к `\\.\pipe\pogrebok-daemon`, кадры
/// [`LengthJsonCodec`](bottle_core::frame::LengthJsonCodec), один
/// запрос — один ответ на соединение.
#[cfg(windows)]
mod client {
    use std::io;
    use std::time::Duration;

    use super::REPLY_TIMEOUT;
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
        #[error("Не удалось подключиться к демону — запущен ли pogrebok-daemon?")]
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

        match tokio::time::timeout(REPLY_TIMEOUT, reader.next()).await {
            Ok(Some(Ok(reply))) => Ok(reply),
            Ok(Some(Err(error))) => Err(ClientError::Frame(error)),
            Ok(None) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout(REPLY_TIMEOUT)),
        }
    }

    /// Подключается к каналу демона с повторами: `ERROR_PIPE_BUSY` и
    /// `ERROR_FILE_NOT_FOUND` в окне [`super::CONNECT_RETRY_WINDOW`] —
    /// стык между экземплярами канала сервера, а не отсутствие демона.
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
                Err(error) => Err(ClientError::Io(error)),
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

    // ---- Надгробия 0.3 (Task 19): управление бутылками переехало в GUI ----

    /// Legacy-команды разбираются (со старыми флагами), но помечены
    /// надгробиями: демону они не отправляются.
    #[test]
    fn tombstone_commands_parse_and_are_marked() {
        for args in [
            vec!["bottle", "list"],
            vec!["bottle", "status"],
            vec!["bottle", "show", "office-a"],
            vec!["bottle", "start", "office-a"],
            vec![
                "bottle", "start", "office-a", "--username", "u", "--password", "p",
            ],
            vec!["bottle", "stop", "office-a"],
            vec!["bottle", "delete", "office-a"],
            vec![
                "bottle", "create", "office-a", "--name", "N", "--kind", "env",
                "--protocol", "wireguard", "--config", r"C:\wg\a.conf",
            ],
        ] {
            let command = parse(&args);
            assert!(command.is_tombstone(), "{args:?} — надгробие 0.3");
        }
    }

    /// Служебные команды — не надгробия и по-прежнему уходят демону.
    #[test]
    fn service_commands_are_not_tombstones() {
        for args in [
            vec!["bottle", "doctor"],
            vec!["bottle", "logs", "export"],
            vec!["bottle", "upgrade"],
            vec!["bottle", "routes", "cleanup"],
            vec!["bottle", "daemon", "stop"],
        ] {
            let command = parse(&args);
            assert!(!command.is_tombstone(), "{args:?} — служебная");
        }
    }

    /// Вывод надгробия: текстовый режим — навигация в приложение; json —
    /// {"Error":…}; оба упоминают «Погребок».
    #[test]
    fn tombstone_output_navigates_to_gui() {
        let text = tombstone_output(false);
        assert!(text.contains("Погребок"), "{text}");
        assert!(text.contains("приложение"), "{text}");

        let json = tombstone_output(true);
        let value: serde_json::Value = serde_json::from_str(&json).expect("корректный JSON");
        assert!(
            value["Error"]["message"]
                .as_str()
                .expect("строка")
                .contains("Погребок"),
            "{json}"
        );
    }

    /// Надгробие завершается кодом 2 (как ошибка разбора): скрипты
    /// отличают его от отказа демона (код 1).
    #[test]
    fn tombstone_exit_code_is_two() {
        assert_eq!(TOMBSTONE_EXIT_CODE, 2);
    }

    // ---- Служебные команды: разбор и маппинг в протокол ----

    /// upgrade, daemon stop и routes cleanup разбираются и отображаются
    /// в запросы.
    #[test]
    fn parses_upgrade_daemon_and_routes_cleanup() {
        assert!(matches!(
            parse(&["bottle", "upgrade"]),
            Command::Upgrade { force: false }
        ));
        assert_eq!(
            parse(&["bottle", "upgrade"]).to_request(),
            CliRequest::Upgrade { force: false }
        );
        assert_eq!(
            parse(&["bottle", "upgrade", "--force"]).to_request(),
            CliRequest::Upgrade { force: true }
        );
        assert_eq!(
            parse(&["bottle", "daemon", "stop"]).to_request(),
            CliRequest::ShutdownDaemon
        );
        assert_eq!(
            parse(&["bottle", "routes", "cleanup"]).to_request(),
            CliRequest::RoutesCleanup
        );
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

    /// Ответы Upgraded/Done: успех; предупреждение cleanup — на stderr.
    #[test]
    fn upgraded_done_replies_are_success() {
        assert_eq!(
            handle_reply(
                &Command::Upgrade { force: false },
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
                CliReply::Path(r"C:\tmp\pogrebok-logs-20261004-1200.zip".to_string())
            ),
            ExitCode::SUCCESS
        );
        assert!(
            render::LOGS_EXPORT_WARNING.contains("просмотрите"),
            "предупреждение о содержимом: {}",
            render::LOGS_EXPORT_WARNING
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

    // ---- `--json`-режим: контракт для GUI ----

    /// Флаг `--json` глобальный: принимается до и после подкоманды.
    #[test]
    fn json_flag_parses_before_and_after_subcommand() {
        for args in [
            vec!["bottle", "--json", "doctor"],
            vec!["bottle", "doctor", "--json"],
            vec!["bottle", "upgrade", "--force", "--json"],
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

    /// I5 ревью: в json-режиме ошибки разбора аргументов (код 2) — тоже
    /// JSON в stdout, а не пустой stdout с текстом в stderr.
    #[test]
    fn parse_error_in_json_mode_is_json_on_stdout() {
        let error = Cli::command().error(
            clap::error::ErrorKind::InvalidSubcommand,
            "unrecognized subcommand 'badcmd'",
        );
        let json = format_parse_error(&["bottle", "--json", "badcmd"], &error)
            .expect("json-режим — JSON-ошибка");
        let value: serde_json::Value = serde_json::from_str(&json).expect("корректный JSON");
        assert_eq!(
            value["Error"]["message"],
            "unrecognized subcommand 'badcmd'"
        );

        // Без --json — None: clap печатает свой вывод сам (код 2).
        let plain = format_parse_error(&["bottle", "badcmd"], &error);
        assert!(plain.is_none());
    }

    /// `--version --json` → `{"version":"…"}`; текстовый формат не меняется.
    #[test]
    fn version_output_text_and_json_shapes() {
        assert_eq!(
            version_output(false),
            format!("pogrebok {}", env!("CARGO_PKG_VERSION"))
        );
        let json = version_output(true);
        assert_eq!(
            json,
            format!("{{\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION"))
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("корректный JSON");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    /// Golden: служебные ответы одной строкой каждый.
    #[test]
    fn json_golden_service_replies() {
        assert_eq!(
            render_json(&CliReply::Path(
                r"C:\tmp\pogrebok-logs-20261004-1200.zip".to_string()
            )),
            r#"{"Path":"C:\\tmp\\pogrebok-logs-20261004-1200.zip"}"#
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
                message: "демон недоступен".to_string()
            }),
            r#"{"Error":{"message":"демон недоступен"}}"#
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

    /// Коды выхода json-режима повторяют текстовый: Error — 1, doctor
    /// с fail — 1 (отчёт всё равно напечатан), остальные успехи — 0.
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
        for reply in [
            CliReply::Done { warning: None },
            CliReply::Path("p.zip".to_string()),
            CliReply::Upgraded {
                previous: None,
                current: "0.3.0".to_string(),
            },
        ] {
            assert_eq!(handle_reply_json(&reply), ExitCode::SUCCESS, "{reply:?}");
        }
    }
}
