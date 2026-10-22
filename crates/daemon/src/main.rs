//! `bottled` — демон BottleForApps.
//!
//! Открывает хранилище профилей в каталоге данных (`--data-dir`, по
//! умолчанию — локальный каталог данных пользователя) и обслуживает канал
//! управления CLI (named pipe `\\.\pipe\bottled`): List/Show читают
//! хранилище, Start/Stop/Status обслуживает менеджер бутылок
//! (`--rootfs` — тарболл аплаенса, по умолчанию кладётся установщиком
//! в `<каталог данных>/appliance/`). Повторный запуск отклоняется
//! single-instance мьютексом; остановка — Ctrl+C.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Целостное состояние демона (0.3), обслуживаемое каналом управления.
///
/// `manager` — менеджер бутылок (реальные Start/Stop); `None` остаётся
/// тестам и dev-сборкам без аплаенса. `wsl` — исполнитель команд WSL
/// (диагностика `doctor`); `None` — там же, где нет менеджера.
/// Профили живут только в `registry` (память; наполняет GUI сообщением
/// SyncProfiles) — дискового хранилища профилей у демона нет.
pub struct Daemon {
    /// Реестр профилей, конфигов и кредов (спека §2).
    pub registry: registry::SharedRegistry,
    /// Менеджер жизненного цикла бутылок.
    pub manager: Option<Arc<manager::BottleManager>>,
    /// Исполнитель команд WSL (`wsl.exe`).
    pub wsl: Option<Arc<dyn appliance::WslRunner>>,
    /// Тарболл rootfs для пере-импорта аплаенса (`upgrade`).
    pub rootfs: Option<PathBuf>,
    /// Токен авторизации GUI (аргумент `--gui-token`).
    pub gui_token: String,
    /// Шина событий для подписчиков (`Subscribe`).
    pub events: tokio::sync::broadcast::Sender<bottle_core::ipc::DaemonEvent>,
    /// Момент старта (монотонные часы; аптайм).
    pub started_at: std::time::Instant,
    /// Момент старта на стенных часах (RFC3339, для Stats).
    pub started_ts: String,
    /// Корень данных (журналы, generation-seed, токен аплаенса).
    pub data_root: PathBuf,
    /// Сэмплер RSS/CPU процесса (Stats, дельта между вызовами).
    pub sampler: stats::ProcessSampler,
}

/// Демон для тестов: пустой реестр, заданный токен, менеджер —
/// параметром; wsl/rootfs отсутствуют.
#[cfg(test)]
pub(crate) fn test_daemon(
    root: &Path,
    token: &str,
    manager: Option<Arc<manager::BottleManager>>,
) -> Arc<Daemon> {
    let (events, _) = tokio::sync::broadcast::channel(64);
    Arc::new(Daemon {
        registry: registry::Registry::new_shared(),
        manager,
        wsl: None,
        rootfs: None,
        gui_token: token.to_string(),
        events,
        started_at: std::time::Instant::now(),
        started_ts: bottle_core::events::now_ts(),
        data_root: root.to_path_buf(),
        sampler: stats::ProcessSampler::new(),
    })
}

// На не-windows диспетчер вызывается только тестами (транспорт — named
// pipe Windows), поэтому модулям нужно allow; на windows цепочка
// main -> serve -> dispatch держит всё живым — это проверяется clippy
// по target x86_64-pc-windows-msvc и windows-джобой CI.
#[allow(dead_code)]
mod appliance;
#[cfg_attr(not(windows), allow(dead_code))]
mod doctor;
#[cfg_attr(not(windows), allow(dead_code))]
mod error;
mod export;
#[cfg_attr(not(windows), allow(dead_code))]
mod host_manager;
#[allow(dead_code)]
mod host_net;
#[allow(dead_code)]
mod instance;
#[cfg_attr(not(windows), allow(dead_code))]
mod launch;
mod logging;
#[allow(dead_code)]
mod manager;
#[cfg_attr(not(windows), allow(dead_code))]
mod pipe;
#[allow(dead_code)]
mod ps_query;
#[allow(dead_code)]
mod registry;
mod session;
mod stats;
#[cfg_attr(not(windows), allow(dead_code))]
mod store;
#[allow(dead_code)]
mod svc_client;
#[cfg(unix)]
mod uds;
#[cfg(all(test, unix))]
#[path = "uds_tests.rs"]
mod uds_tests;

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_cli_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("bottled: {message}");
            eprintln!(
                "использование: bottled --gui-token <64 hex> [--data-dir <PATH>] [--rootfs <PATH>]"
            );
            return ExitCode::FAILURE;
        }
    };

    // Файловый JSONL-журнал в `<data>/logs` (ротация ежедневно,
    // хранение 30 дней). Инициализация до всего, что логируется;
    // сбой не фатален — демон работает с stdout-логами.
    if let Err(error) = logging::init_file_logging(&args.data_dir.join("logs"), "bottled") {
        eprintln!("bottled: файловый журнал недоступен ({error}); пишу только в консоль");
        init_tracing();
    }

    if !instance::acquire_single_instance(&args.data_dir) {
        eprintln!("Демон BottleForApps уже запущен");
        return ExitCode::FAILURE;
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let rootfs = resolve_rootfs(args.rootfs, &args.data_dir, exe_dir.as_deref());
    if let Err(error) = serve_daemon(args.data_dir, rootfs, args.gui_token, shutdown_on_ctrl_c()).await {
        tracing::error!(%error, "bottled завершился с ошибкой");
        eprintln!("bottled: {error}");
        return ExitCode::FAILURE;
    }
    println!("Демон BottleForApps остановлен");
    ExitCode::SUCCESS
}

/// Открывает хранилище и обслуживает канал управления до отмены `shutdown`.
///
/// На Windows создаётся менеджер бутылок с реальным подключением
/// к аплаенсу (импорт/токен/handshake — `appliance::ensure_appliance`).
///
/// `gui_token` — токен авторизации GUI (спека §5): демон 0.3 запускает
/// только GUI, токен передаётся аргументом `--gui-token`.
pub async fn serve_daemon(
    data_dir: PathBuf,
    rootfs_tar: PathBuf,
    gui_token: String,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    // Каталог данных с правами 0700 (сторонняя учётка не читает журналы
    // и токен аплаенса); хранилища профилей на диске в 0.3 нет — открытие
    // Store осталось ради подготовки каталога (Task 6 заменит paths-модулем).
    let store = store::Store::open(&data_dir)?;
    let data_root = store.data_root().to_path_buf();
    println!("Демон BottleForApps запущен");
    println!("Каталог данных: {}", data_root.display());
    println!("Rootfs аплаенса: {}", rootfs_tar.display());

    let registry = crate::registry::Registry::new_shared();
    let (events, _) = tokio::sync::broadcast::channel(256);

    #[cfg(windows)]
    {
        println!("Канал управления: {}", bottle_core::ipc::DAEMON_PIPE);
        let wsl: Arc<dyn appliance::WslRunner> = Arc::new(appliance::RealWsl);
        let connect = Arc::new(manager::RealAgentConnect::new(
            Arc::clone(&wsl),
            data_dir,
            rootfs_tar.clone(),
            manager::AGENT_CONNECT_TIMEOUT,
        ));
        // Менеджер читает профили из in-memory реестра (0.3): наполняет
        // его GUI сообщением SyncProfiles после подключения.
        let manager = Arc::new(manager::BottleManager::new(
            registry.clone(),
            data_root.clone(),
            connect,
        ));
        // Фоновый цикл DNS-перепроверки host-бутылок (спека §7.2, шаг 5)
        // живёт до остановки демона.
        manager.spawn_host_dns_recheck(shutdown.clone());
        // Фоновый цикл живучести (T11, спека §9): поллинг агента,
        // авто-восстановление env-бутылок, живость host-интерфейсов.
        manager.spawn_health_loop(shutdown.clone(), manager::HEALTH_CHECK_PERIOD);
        let daemon = Arc::new(Daemon {
            registry,
            manager: Some(manager),
            wsl: Some(wsl),
            rootfs: Some(rootfs_tar),
            gui_token,
            events,
            started_at: std::time::Instant::now(),
            started_ts: bottle_core::events::now_ts(),
            data_root,
            sampler: stats::ProcessSampler::new(),
        });
        pipe::serve(bottle_core::ipc::DAEMON_PIPE, daemon, shutdown).await
    }

    #[cfg(unix)]
    {
        // macOS/Unix-транспорт — UDS `<data>/daemon.sock` (M4 Task 7);
        // протокол и диспетчер те же, что у named pipe Windows. Менеджер
        // бутылок на macOS подключается в Task 10 (VzRunner); до него
        // диспетчер честно отвечает переходной ошибкой.
        let socket = bottle_core::ipc::daemon_socket_path(&data_root);
        println!("Канал управления: {}", socket.display());
        let daemon = Arc::new(Daemon {
            registry,
            manager: None,
            wsl: None,
            rootfs: None,
            gui_token,
            events,
            started_at: std::time::Instant::now(),
            started_ts: bottle_core::events::now_ts(),
            data_root,
            sampler: stats::ProcessSampler::new(),
        });
        uds::serve(&socket, daemon, shutdown).await
    }

    #[cfg(not(any(windows, unix)))]
    {
        // Транспорта нет: dev-запуск проверяет старт и остановку.
        let _ = (registry, events, gui_token);
        println!("Канал управления недоступен на этой платформе; ожидаю Ctrl+C");
        shutdown.cancelled().await;
        Ok(())
    }
}

/// Аргументы командной строки демона.
struct CliArgs {
    /// Каталог данных (журналы, токен аплаенса).
    data_dir: PathBuf,
    /// Явный тарболл rootfs (`--rootfs`); `None` — разрешается
    /// [`resolve_rootfs`] (данные → каталог exe).
    rootfs: Option<PathBuf>,
    /// Токен авторизации GUI (`--gui-token`, 64 hex): демон 0.3
    /// запускает только GUI, без токена управление закрыто (спека §5).
    gui_token: String,
}

/// Имя файла rootfs внутри `<data_dir>/appliance/` (кладётся установщиком).
const ROOTFS_FILE_NAME: &str = "bottleforapps-rootfs.tar.gz";

/// Разбирает аргументы командной строки: `--gui-token <64 hex>` /
/// `--data-dir <PATH>` / `--rootfs <PATH>` (также формы через `=`).
/// Токен обязателен (демон 0.3 спавнится из GUI); без `--data-dir` —
/// каталог данных по умолчанию; без `--rootfs` —
/// `<data_dir>/appliance/<ROOTFS_FILE_NAME>` (dev-запуски передают
/// `--rootfs` явно).
fn parse_cli_args<I>(args: I) -> Result<CliArgs, String>
where
    I: Iterator<Item = OsString>,
{
    let flag_value = |args: &mut I, flag: &str| -> Result<OsString, String> {
        args.next().ok_or_else(|| format!("для {flag} нужно значение"))
    };
    let strip = |arg: &OsString, flag: &str| -> Option<OsString> {
        arg.to_str()
            .and_then(|a| a.strip_prefix(flag))
            .map(OsString::from)
    };

    let mut data_dir = None;
    let mut rootfs = None;
    let mut gui_token = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg.as_os_str() == OsStr::new("--data-dir") {
            data_dir = Some(PathBuf::from(flag_value(&mut args, "--data-dir")?));
        } else if let Some(value) = strip(&arg, "--data-dir=") {
            data_dir = Some(PathBuf::from(value));
        } else if arg.as_os_str() == OsStr::new("--rootfs") {
            rootfs = Some(PathBuf::from(flag_value(&mut args, "--rootfs")?));
        } else if let Some(value) = strip(&arg, "--rootfs=") {
            rootfs = Some(PathBuf::from(value));
        } else if arg.as_os_str() == OsStr::new("--gui-token") {
            gui_token = Some(flag_value(&mut args, "--gui-token")?);
        } else if let Some(value) = strip(&arg, "--gui-token=") {
            gui_token = Some(value);
        } else {
            return Err(format!("неизвестный аргумент: {}", arg.to_string_lossy()));
        }
    }

    let data_dir = data_dir.or_else(default_data_dir).ok_or_else(|| {
        "не удалось определить локальный каталог данных; укажите --data-dir <PATH>".to_string()
    })?;
    let gui_token = gui_token
        .ok_or_else(|| "нужен --gui-token <64 hex> (демон запускается из приложения)".to_string())?
        .to_str()
        .ok_or_else(|| "токен GUI — не UTF-8".to_string())?
        .to_string();
    if gui_token.len() != 64
        || !gui_token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err("токен GUI — 64 шестнадцатеричных символа в нижнем регистре".to_string());
    }
    Ok(CliArgs {
        data_dir,
        rootfs,
        gui_token,
    })
}

/// Каталог данных по умолчанию: `<локальный каталог данных>/BottleForApps`.
fn default_data_dir() -> Option<PathBuf> {
    bottle_core::ipc::default_data_dir()
}

/// Разрешает путь rootfs-тарболла (рулинг M3): явный `--rootfs` →
/// `<data>/appliance/<ROOTFS_FILE_NAME>` (если файл существует; MSI
/// кладёт его туда) → `<exe_dir>/appliance/<ROOTFS_FILE_NAME>`
/// (portable-сборка: rootfs рядом с exe) → `<data>/appliance/…` как
/// до M3 — сообщения об отсутствии продолжают указывать на каталог
/// данных. `exe_dir` — каталог исполняемого файла (`None`, если путь
/// недоступен).
fn resolve_rootfs(explicit: Option<PathBuf>, data_dir: &Path, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(explicit) = explicit {
        return explicit;
    }
    let data_rootfs = data_dir.join("appliance").join(ROOTFS_FILE_NAME);
    if data_rootfs.is_file() {
        return data_rootfs;
    }
    if let Some(exe_rootfs) = exe_dir.map(|dir| dir.join("appliance").join(ROOTFS_FILE_NAME)) {
        if exe_rootfs.is_file() {
            return exe_rootfs;
        }
    }
    data_rootfs
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

/// Инициализирует логирование (fallback без файлового журнала):
/// уровень из `RUST_LOG`, по умолчанию `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23";

    fn args(list: &[&str]) -> std::vec::IntoIter<OsString> {
        list.iter()
            .map(|s| OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn data_dir_accepts_space_and_equals_forms() {
        assert_eq!(
            parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir", "/tmp/bfa"]))
                .unwrap()
                .data_dir,
            PathBuf::from("/tmp/bfa")
        );
        assert_eq!(
            parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir=/tmp/bfa2"]))
                .unwrap()
                .data_dir,
            PathBuf::from("/tmp/bfa2")
        );
    }

    #[test]
    fn rootfs_accepts_space_and_equals_forms() {
        assert_eq!(
            parse_cli_args(args(&[
                "--gui-token",
                TOKEN,
                "--data-dir",
                "/tmp/bfa",
                "--rootfs",
                "/tmp/rootfs.tar.gz"
            ]))
            .unwrap()
            .rootfs,
            Some(PathBuf::from("/tmp/rootfs.tar.gz"))
        );
        assert_eq!(
            parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir", "/tmp/bfa", "--rootfs=/tmp/r.tar"]))
                .unwrap()
                .rootfs,
            Some(PathBuf::from("/tmp/r.tar"))
        );
    }

    /// Без `--rootfs` парсер не выбирает место сам — резолвер
    /// [`resolve_rootfs`] разбирается с данными/exe-каталогом.
    #[test]
    fn rootfs_absent_is_none_after_parse() {
        let parsed = parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir", "/tmp/bfa"])).unwrap();
        assert_eq!(parsed.rootfs, None);
    }

    #[test]
    fn flags_without_value_are_error() {
        assert!(parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir"])).is_err());
        assert!(
            parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir", "/tmp/bfa", "--rootfs"]))
                .is_err(),
            "для --rootfs нужен путь"
        );
    }

    #[test]
    fn unknown_argument_is_error() {
        assert!(parse_cli_args(args(&["--gui-token", TOKEN, "--verbose"])).is_err());
    }

    /// Токен GUI обязателен (демон 0.3 спавнится из приложения);
    /// форма — 64 hex в нижнем регистре.
    #[test]
    fn gui_token_is_required_and_validated() {
        assert!(
            parse_cli_args(args(&["--data-dir", "/tmp/bfa"])).is_err(),
            "без токена запуск запрещён"
        );
        let parsed = parse_cli_args(args(&["--gui-token", TOKEN, "--data-dir", "/tmp/bfa"]))
            .expect("валидный токен принят");
        assert_eq!(parsed.gui_token, TOKEN);
        // Форма через `=`.
        let parsed =
            parse_cli_args(args(&["--gui-token=ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23ab10cd23"]))
                .expect("форма через =");
        assert_eq!(parsed.gui_token, TOKEN);
        for bad in ["короткий", &TOKEN.to_uppercase(), &TOKEN[..63], &format!("{TOKEN}0")] {
            assert!(
                parse_cli_args(args(&["--gui-token", bad])).is_err(),
                "невалидный токен отклонён: {bad}"
            );
        }
    }

    /// Резолвер rootfs (рулинг M3): явный `--rootfs` → `<data>/appliance`
    /// (если файл есть) → `<exe_dir>/appliance` (portable-установка,
    /// если файл есть) → `<data>/appliance` по умолчанию (текущее
    /// поведение: сообщения об ошибках указывают на каталог данных).
    #[test]
    fn resolve_rootfs_prefers_explicit_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let exe = tmp.path().join("exe");
        assert_eq!(
            resolve_rootfs(
                Some(PathBuf::from("/custom/rootfs.tar.gz")),
                &data,
                Some(&exe)
            ),
            PathBuf::from("/custom/rootfs.tar.gz"),
            "явный флаг не переопределяется никогда"
        );
    }

    #[test]
    fn resolve_rootfs_prefers_data_over_exe_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let exe = tmp.path().join("exe");
        std::fs::create_dir_all(data.join("appliance")).unwrap();
        std::fs::create_dir_all(exe.join("appliance")).unwrap();
        std::fs::write(data.join("appliance").join(ROOTFS_FILE_NAME), "data").unwrap();
        std::fs::write(exe.join("appliance").join(ROOTFS_FILE_NAME), "exe").unwrap();

        assert_eq!(
            resolve_rootfs(None, &data, Some(&exe)),
            data.join("appliance").join(ROOTFS_FILE_NAME),
            "MSI кладёт rootfs в данные — приоритет"
        );
    }

    /// Portable: rootfs рядом с exe, в данных его нет.
    #[test]
    fn resolve_rootfs_falls_back_to_exe_dir_for_portable() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let exe = tmp.path().join("exe");
        std::fs::create_dir_all(exe.join("appliance")).unwrap();
        std::fs::write(exe.join("appliance").join(ROOTFS_FILE_NAME), "exe").unwrap();

        assert_eq!(
            resolve_rootfs(None, &data, Some(&exe)),
            exe.join("appliance").join(ROOTFS_FILE_NAME),
            "portable-сборка: rootfs у exe"
        );
    }

    /// Нигде нет — каталог данных (единственное место, куда смотрит
    /// установка/ошибки).
    #[test]
    fn resolve_rootfs_defaults_to_data_dir_when_nowhere() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let exe = tmp.path().join("exe");

        assert_eq!(
            resolve_rootfs(None, &data, Some(&exe)),
            data.join("appliance").join(ROOTFS_FILE_NAME)
        );
        // Без exe_dir (current_exe недоступен) — то же самое.
        assert_eq!(
            resolve_rootfs(None, &data, None),
            data.join("appliance").join(ROOTFS_FILE_NAME)
        );
    }

    #[test]
    fn no_data_dir_falls_back_to_default() {
        let parsed = parse_cli_args(args(&["--gui-token", TOKEN]))
            .expect("локальный каталог данных определён в тестовом окружении");
        assert!(parsed.data_dir.ends_with("BottleForApps"));
    }
}
