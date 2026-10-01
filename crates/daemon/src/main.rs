//! `bottled` — демон BottleForApps.
//!
//! Открывает хранилище профилей в каталоге данных (`--data-dir`, по
//! умолчанию — локальный каталог данных пользователя) и обслуживает канал
//! управления CLI (named pipe `\\.\pipe\bottled`): List/Show читают
//! хранилище, Start/Stop/Status подключаются в Task 16. Повторный запуск
//! отклоняется single-instance мьютексом; остановка — Ctrl+C.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(windows)]
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Целостное состояние демона, обслуживаемое каналом управления.
///
/// Поле менеджера бутылок (реальные Start/Stop/Status) подключается
/// в Task 16; до него команды управления отвечают переходной ошибкой.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Daemon {
    /// Хранилище профилей бутылок.
    pub store: store::Store,
}

// На не-windows диспетчер вызывается только тестами (транспорт — named
// pipe Windows), поэтому модулям нужно allow; на windows цепочка
// main -> serve -> dispatch держит всё живым — это проверяется clippy
// по target x86_64-pc-windows-msvc и windows-джобой CI.
#[cfg_attr(not(windows), allow(dead_code))]
mod error;
mod instance;
#[cfg_attr(not(windows), allow(dead_code))]
mod pipe;
#[cfg_attr(not(windows), allow(dead_code))]
mod store;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let data_dir = match parse_data_dir(std::env::args_os().skip(1)) {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("bottled: {message}");
            eprintln!("использование: bottled [--data-dir <PATH>]");
            return ExitCode::FAILURE;
        }
    };

    if !instance::acquire_single_instance() {
        eprintln!("Демон BottleForApps уже запущен");
        return ExitCode::FAILURE;
    }

    if let Err(error) = serve_daemon(data_dir, shutdown_on_ctrl_c()).await {
        tracing::error!(%error, "bottled завершился с ошибкой");
        eprintln!("bottled: {error}");
        return ExitCode::FAILURE;
    }
    println!("Демон BottleForApps остановлен");
    ExitCode::SUCCESS
}

/// Открывает хранилище и обслуживает канал управления до отмены `shutdown`.
///
/// Шов для Task 16/17: менеджер бутылок встанет внутрь [`Daemon`], а цикл
/// [`pipe::serve`] и диспетчер останутся прежними.
pub async fn serve_daemon(data_dir: PathBuf, shutdown: CancellationToken) -> std::io::Result<()> {
    let store = store::Store::open(&data_dir)?;
    println!("Демон BottleForApps запущен");
    println!("Каталог данных: {}", store.data_root().display());

    #[cfg(windows)]
    {
        println!("Канал управления: {}", bottle_core::ipc::DAEMON_PIPE);
        let daemon = Arc::new(Daemon { store });
        pipe::serve(bottle_core::ipc::DAEMON_PIPE, daemon, shutdown).await
    }

    #[cfg(not(windows))]
    {
        // Named pipe — транспорт Windows. Dev-запуск на других платформах
        // проверяет старт (хранилище, single-instance) и корректную
        // остановку по Ctrl+C, канал не обслуживая.
        println!("Канал управления доступен только на Windows; ожидаю Ctrl+C");
        shutdown.cancelled().await;
        Ok(())
    }
}

/// Разбирает аргументы командной строки: `--data-dir <PATH>` или
/// `--data-dir=<PATH>`. Без флага — каталог данных по умолчанию.
fn parse_data_dir<I>(args: I) -> Result<PathBuf, String>
where
    I: Iterator<Item = OsString>,
{
    let mut data_dir = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg.as_os_str() == OsStr::new("--data-dir") {
            let value = args
                .next()
                .ok_or_else(|| "для --data-dir нужен путь".to_string())?;
            data_dir = Some(PathBuf::from(value));
        } else if let Some(value) = arg.to_str().and_then(|a| a.strip_prefix("--data-dir=")) {
            data_dir = Some(PathBuf::from(value));
        } else {
            return Err(format!("неизвестный аргумент: {}", arg.to_string_lossy()));
        }
    }
    match data_dir {
        Some(dir) => Ok(dir),
        None => default_data_dir().ok_or_else(|| {
            "не удалось определить локальный каталог данных; укажите --data-dir <PATH>".to_string()
        }),
    }
}

/// Каталог данных по умолчанию: `<локальный каталог данных>/BottleForApps`.
fn default_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|base| base.join("BottleForApps"))
}

/// Токен остановки, отменяемый по Ctrl+C.
fn shutdown_on_ctrl_c() -> CancellationToken {
    let shutdown = CancellationToken::new();
    let watcher = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("получен Ctrl+C, останавливаю демон");
            watcher.cancel();
        }
    });
    shutdown
}

/// Инициализирует логирование: уровень из `RUST_LOG`, по умолчанию `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> std::vec::IntoIter<OsString> {
        list.iter()
            .map(|s| OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn data_dir_accepts_space_and_equals_forms() {
        assert_eq!(
            parse_data_dir(args(&["--data-dir", "/tmp/bfa"])).unwrap(),
            PathBuf::from("/tmp/bfa")
        );
        assert_eq!(
            parse_data_dir(args(&["--data-dir=/tmp/bfa2"])).unwrap(),
            PathBuf::from("/tmp/bfa2")
        );
    }

    #[test]
    fn data_dir_without_value_is_error() {
        assert!(parse_data_dir(args(&["--data-dir"])).is_err());
    }

    #[test]
    fn unknown_argument_is_error() {
        assert!(parse_data_dir(args(&["--verbose"])).is_err());
    }

    #[test]
    fn no_args_fall_back_to_default_data_dir() {
        let dir = parse_data_dir(args(&[]))
            .expect("локальный каталог данных определён в тестовом окружении");
        assert!(dir.ends_with("BottleForApps"));
    }
}
