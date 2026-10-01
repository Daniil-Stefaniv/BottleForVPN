//! `bottle` — тонкий CLI-клиент демона BottleForApps (спека §4.4).
//!
//! Вся бизнес-логика живёт в демоне: CLI разбирает аргументы (clap),
//! отправляет [`CliRequest`] по named pipe `\\.\pipe\bottled` и рендерит
//! [`CliReply`] в русский вывод. Рендер вынесен в [`render`] и покрыт
//! unit-тестами на любой платформе; транспорт — только на Windows.

mod render;

use std::process::ExitCode;
use std::time::Duration;

use bottle_core::ipc::{CliReply, CliRequest};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bottle",
    about = "BottleForApps — управление бутылками",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

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

    // Транспорт демона существует только на Windows (named pipe, спека §4.4);
    // рендер и разбор аргументов при этом кроссплатформенны и тестируются.
    #[cfg(not(windows))]
    {
        let _ = cli;
        eprintln!("bottle работает только на Windows (демон недоступен на этой платформе)");
        ExitCode::FAILURE
    }

    #[cfg(windows)]
    {
        let request = cli.command.to_request();
        match client::exchange(&request).await {
            Ok(reply) => handle_reply(&cli.command, reply),
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        }
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
            None => {
                eprintln!("Бутылка не найдена: {id}");
                ExitCode::FAILURE
            }
        },
        (Command::Start { .. }, CliReply::Started { id }) => {
            println!("Бутылка {id} поднята");
            ExitCode::SUCCESS
        }
        (Command::Stop { .. }, CliReply::Stopped { id }) => {
            println!("Бутылка {id} опущена");
            ExitCode::SUCCESS
        }
        (Command::Status, CliReply::Status { bottles }) => {
            println!("{}", render::render_status(bottles));
            ExitCode::SUCCESS
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

    /// Окно повторных попыток подключения. Демон обслуживает клиентов по
    /// одному и пересоздаёт экземпляр канала между соединениями — в этот
    /// момент канал «занят» или «не существует», и это ожидаемо. Пока идёт
    /// чужой длинный `start`, канал занят всю его длительность, поэтому
    /// окно должно покрывать холодный старт целиком: импорт аплаенса +
    /// подключение агента + handshake туннеля. 60 с хватает с запасом.
    const CONNECT_RETRY_WINDOW: Duration = Duration::from_secs(60);
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
        let deadline = tokio::time::Instant::now() + CONNECT_RETRY_WINDOW;
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
    }

    #[test]
    fn parses_every_subcommand() {
        assert!(matches!(parse(&["bottle", "list"]), Command::List));
        assert!(matches!(parse(&["bottle", "status"]), Command::Status));
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

    #[test]
    fn id_is_required_for_show_start_stop() {
        for command in ["show", "start", "stop"] {
            assert!(
                Cli::try_parse_from(["bottle", command]).is_err(),
                "«{command}» без id должен отказывать"
            );
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
}
