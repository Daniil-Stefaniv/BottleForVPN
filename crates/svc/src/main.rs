//! `bottle-svc` — служба маршрутов BottleForApps (Windows).
//!
//! Привилегированная служба, удерживающая точечные host-маршруты
//! (`/32`/`/128`) env-бутылок через физический шлюз. Без аргументов
//! (или с `run`) процесс запускается как служба под управлением SCM;
//! `--console` — отладка без SCM; `--install`/`--uninstall` —
//! административные операции над реестром служб.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

// Реквизиты службы используются windows-обвязкой SCM ниже; на других
// платформах каркас проверяется только unit-тестами парсера (паттерн
// daemon: cfg_attr-allow вместо cfg на каждом использовании).
/// Имя службы в реестре SCM (спека §11 наследует имя из v1-спеки).
#[cfg_attr(not(windows), allow(dead_code))]
const SERVICE_NAME: &str = "BottleForAppsRouteHelper";
/// Отображаемое имя службы.
#[cfg_attr(not(windows), allow(dead_code))]
const SERVICE_DISPLAY_NAME: &str = "BottleForApps Route Helper";
/// Русское описание службы для реестра SCM.
#[cfg_attr(not(windows), allow(dead_code))]
const SERVICE_DESCRIPTION: &str =
    "Служба маршрутов BottleForApps: точечные маршруты host-бутылок через физический шлюз";

/// Режим запуска, выбираемый аргументами командной строки.
#[derive(Debug)]
enum Mode {
    /// Штатный режим службы под управлением SCM (без аргументов, `run`).
    Run,
    /// Отладочный консольный процесс без SCM (`--console`).
    Console,
    /// Установка службы (`--install`, требует администратора).
    Install,
    /// Удаление службы (`--uninstall`, требует администратора).
    Uninstall,
}

fn main() -> ExitCode {
    let mode = match parse_cli_args(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("bottle-svc: {message}");
            eprintln!("использование: bottle-svc [run|--console|--install|--uninstall]");
            return ExitCode::FAILURE;
        }
    };
    dispatch(mode)
}

/// Разбирает аргументы командной строки: без аргументов или `run` —
/// режим службы; `--console`/`--install`/`--uninstall` — отладка и
/// административные операции. Первый токен проверяется на известность
/// раньше проверки лишних аргументов (диагностика указывает на сам
/// неверный флаг), прочее и лишние аргументы — ошибка.
fn parse_cli_args<I>(args: I) -> Result<Mode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut args = args;
    let Some(arg) = args.next() else {
        return Ok(Mode::Run);
    };
    let mode = if arg == OsStr::new("run") {
        Mode::Run
    } else if arg == OsStr::new("--console") {
        Mode::Console
    } else if arg == OsStr::new("--install") {
        Mode::Install
    } else if arg == OsStr::new("--uninstall") {
        Mode::Uninstall
    } else {
        return Err(format!("неизвестный аргумент: {}", arg.to_string_lossy()));
    };
    if let Some(extra) = args.next() {
        return Err(format!("лишний аргумент: {}", extra.to_string_lossy()));
    }
    Ok(mode)
}

/// Запускает выбранный режим. Обвязка SCM доступна только на Windows:
/// на других платформах консольный режим служит dev-проверкой каркаса
/// (как у `bottled`), служебные режимы — отказ с русским текстом.
#[cfg(windows)]
fn dispatch(mode: Mode) -> ExitCode {
    match mode {
        Mode::Run => win_service::run_as_service(),
        Mode::Console => run_console(),
        Mode::Install => win_service::install(),
        Mode::Uninstall => win_service::uninstall(),
    }
}

#[cfg(not(windows))]
fn dispatch(mode: Mode) -> ExitCode {
    match mode {
        Mode::Console => run_console(),
        Mode::Run | Mode::Install | Mode::Uninstall => {
            eprintln!(
                "bottle-svc: операции службы доступны только на Windows; \
                 для отладки запустите с --console"
            );
            ExitCode::FAILURE
        }
    }
}

/// Отладочный режим без SCM: полный цикл службы (журнал, сверка,
/// pipe-сервер), остановка по Ctrl+C.
fn run_console() -> ExitCode {
    init_tracing();
    println!("bottle-svc: консольный режим отладки (без SCM)");
    println!(
        "bottle-svc: журнал маршрутов: {}",
        svc::journal::default_journal_path().display()
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "не удалось создать tokio runtime");
            eprintln!("bottle-svc: не удалось создать tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let shutdown = CancellationToken::new();
    let watcher = shutdown.clone();
    runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            println!("получен Ctrl+C, останавливаю bottle-svc");
            watcher.cancel();
        }
    });
    let outcome = runtime.block_on(run_server(shutdown));
    if let Err(error) = outcome {
        eprintln!("bottle-svc: {error}");
        return ExitCode::FAILURE;
    }
    println!("bottle-svc остановлен");
    ExitCode::SUCCESS
}

/// Внутренний сервер службы: открыть журнал → сверка с ОС →
/// pipe-сервер на [`bottle_core::svcrpc::SVC_PIPE`] до сигнала
/// остановки. Отказ журнала фатален: без него служба не может
/// гарантировать контракт «маршруты не переживают смерть демона».
async fn run_server(shutdown: CancellationToken) -> Result<(), String> {
    let journal_path = svc::journal::default_journal_path();
    let mut store = svc::journal::RouteStore::open(&journal_path)
        .map_err(|error| format!("журнал маршрутов недоступен: {error}"))?;
    tracing::info!(journal = %journal_path.display(), "журнал маршрутов открыт");

    // При старте службы живых сессий нет: все live-маршруты из журнала
    // сверяются с ОС (сироты удаляются, отсутствующие вычищаются).
    let report = svc::journal::reconcile(&mut store, |_| false).await;
    tracing::info!(
        removed = report.removed,
        vanished = report.vanished,
        kept = report.kept,
        undecided = report.undecided,
        "сверка журнала при старте завершена"
    );

    let store = Arc::new(tokio::sync::Mutex::new(store));
    svc::pipe::serve(bottle_core::svcrpc::SVC_PIPE, store, shutdown)
        .await
        .map_err(|error| format!("pipe-сервер службы маршрутов: {error}"))
}

/// Инициализирует логирование: уровень из `RUST_LOG`, по умолчанию
/// `info` (единый паттерн с `bottled`).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Обвязка Windows-службы: диспетчер SCM, статусы, install/uninstall.
#[cfg(windows)]
mod win_service {
    use std::ffi::OsString;
    use std::process::ExitCode;
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, Error as ScmError};

    use super::run_server;
    use crate::{SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME, SERVICE_NAME};

    /// Win32 ERROR_ACCESS_DENIED — установка/удаление без прав
    /// администратора падает именно с ним.
    const ERROR_ACCESS_DENIED: i32 = 5;
    /// Win32 ERROR_SERVICE_EXISTS — повторная установка уже
    /// установленной службы.
    const ERROR_SERVICE_EXISTS: i32 = 1070;
    /// Win32 ERROR_SERVICE_DOES_NOT_EXIST — удаление неустановленной
    /// службы.
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    // Низкоуровневая точка входа службы: парсит argv и вызывает
    // service_main в отдельном потоке SCM.
    define_windows_service!(ffi_service_main, service_main);

    /// Запускает службу: регистрирует `ffi_service_main` в диспетчере
    /// SCM и блокирует поток до остановки службы. Требует запуска
    /// от SCM (`sc start`/автозапуск); иначе — отказ с подсказкой.
    pub fn run_as_service() -> ExitCode {
        // У процесса службы нет stdout/stderr — только трейсинг
        // (журнал маршрутов добавит задача 5).
        super::init_tracing();
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                // Display ошибок crate'а малосодержателен («IO error in
                // winapi call»), поэтому добавляем код ОС (запуск не из
                // SCM — ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, 1063).
                let detail = match &error {
                    ScmError::Winapi(io) => match io.raw_os_error() {
                        Some(code) => format!("код ОС {code}"),
                        None => io.to_string(),
                    },
                    other => other.to_string(),
                };
                eprintln!("bottle-svc: не удалось запуститься как служба: {detail}");
                eprintln!("для отладки вне SCM запустите с --console");
                ExitCode::FAILURE
            }
        }
    }

    /// Точка входа службы: вызывается SCM в отдельном потоке.
    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            tracing::error!(%error, "служба маршрутов завершилась с ошибкой");
            std::process::exit(1);
        }
    }

    /// Регистрирует обработчик событий SCM и гоняет полный цикл
    /// состояний. Любая ошибка ПОСЛЕ регистрации доложивается SCM
    /// best-effort переводом в Stopped перед аварийным выходом (иначе
    /// служба зависает в StartPending до таймаута SCM).
    fn run_service() -> Result<(), Box<dyn std::error::Error>> {
        let shutdown = CancellationToken::new();
        let stop_watcher = shutdown.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                // Обязателен к приёму даже без реализации.
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop => {
                    stop_watcher.cancel();
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };
        // Ошибка register — статуса ещё нет, доложить SCM некому.
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

        // ServiceStatusHandle — Copy: передача по значению не лишает
        // возможности доложить Stopped в ветке ошибки ниже.
        let result = serve_until_stop(status_handle, shutdown);
        if result.is_err() {
            if let Err(report_error) = status_handle.set_service_status(stopped_status()) {
                tracing::warn!(error = %report_error, "не удалось доложить SCM о Stopped");
            }
        }
        result
    }

    /// Полный цикл состояний после register: StartPending → Running →
    /// внутренний сервер (журнал, сверка, pipe) → Stopped; сигнал Stop
    /// пробуждает сервер, открытые сессии закрываются с удалением их
    /// маршрутов из ОС.
    fn serve_until_stop(
        status_handle: windows_service::service_control_handler::ServiceStatusHandle,
        shutdown: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StartPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let server_error = runtime.block_on(async {
            if let Err(error) = status_handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            }) {
                tracing::error!(%error, "не удалось перевести службу в Running");
                return None;
            }
            tracing::info!("служба маршрутов BottleForApps запущена");
            run_server(shutdown).await.err()
        });

        status_handle.set_service_status(stopped_status())?;
        match server_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// Статус «служба остановлена» для доклада SCM.
    fn stopped_status() -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        }
    }

    /// `--install`: устанавливает службу (требует администратора).
    pub fn install() -> ExitCode {
        match install_service() {
            Ok(()) => {
                println!(
                    "Служба «{SERVICE_DISPLAY_NAME}» ({SERVICE_NAME}) установлена: \
                     автозапуск, binary <current_exe> run"
                );
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("bottle-svc: {message}");
                ExitCode::FAILURE
            }
        }
    }

    fn install_service() -> Result<(), String> {
        let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
        let manager = ServiceManager::local_computer(None::<&str>, manager_access)
            .map_err(|e| describe_scm_error("не удалось подключиться к диспетчеру служб", &e))?;

        let executable_path = std::env::current_exe()
            .map_err(|e| format!("не удалось определить путь исполняемого файла: {e}"))?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path,
            // SCM запускает службу как `bottle-svc.exe run`.
            launch_arguments: vec![OsString::from("run")],
            dependencies: vec![],
            // LocalSystem: операции над маршрутами требуют SYSTEM.
            account_name: None,
            account_password: None,
        };
        let service = manager
            .create_service(&info, ServiceAccess::CHANGE_CONFIG)
            .map_err(|e| describe_scm_error("не удалось установить службу", &e))?;
        // Служба уже создана: частичный сбой дальше честно сообщает об
        // этом — повторный --install увидит переведённый
        // ERROR_SERVICE_EXISTS, а не «сырую» ошибку; --uninstall удалит.
        if let Err(e) = service.set_description(SERVICE_DESCRIPTION) {
            return Err(format!(
                "служба установлена, но не удалось задать описание ({}); \
                 служба осталась в системе — повторная установка сообщит, \
                 что она уже установлена, --uninstall удалит её",
                describe_scm_error("set_description", &e)
            ));
        }
        Ok(())
    }

    /// `--uninstall`: останавливает (если работает) и удаляет службу.
    pub fn uninstall() -> ExitCode {
        match uninstall_service() {
            Ok(()) => {
                println!(
                    "Служба «{SERVICE_DISPLAY_NAME}» ({SERVICE_NAME}) помечена на удаление \
                     (окончательно удалится после остановки процесса службы)"
                );
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("bottle-svc: {message}");
                ExitCode::FAILURE
            }
        }
    }

    fn uninstall_service() -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| {
            describe_scm_error("не удалось подключиться к диспетчеру служб", &e)
        })?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
            )
            .map_err(|e| match &e {
                ScmError::Winapi(io) if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                    format!("служба {SERVICE_NAME} не установлена")
                }
                _ => describe_scm_error("не удалось открыть службу", &e),
            })?;

        // Не остановлена — останавливаем; удаление завершится, когда
        // процесс службы выйдет.
        let running = service
            .query_status()
            .map(|status| status.current_state != ServiceState::Stopped)
            .unwrap_or(false);
        if running {
            service
                .stop()
                .map_err(|e| describe_scm_error("не удалось остановить службу", &e))?;
        }
        service
            .delete()
            .map_err(|e| describe_scm_error("не удалось удалить службу", &e))?;
        drop(service);
        Ok(())
    }

    /// Переводит ошибки SCM в русские сообщения; отказ в доступе —
    /// явное требование прав администратора, повторная установка —
    /// сообщение, что служба уже установлена.
    fn describe_scm_error(context: &str, error: &ScmError) -> String {
        if let ScmError::Winapi(io) = error {
            if io.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                return format!("{context}: операция требует прав администратора");
            }
            if io.raw_os_error() == Some(ERROR_SERVICE_EXISTS) {
                return format!("{context}: служба {SERVICE_NAME} уже установлена");
            }
        }
        format!("{context}: {error}")
    }
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
    fn no_args_default_to_service_mode() {
        assert!(matches!(parse_cli_args(args(&[])), Ok(Mode::Run)));
    }

    #[test]
    fn run_selects_service_mode() {
        assert!(matches!(parse_cli_args(args(&["run"])), Ok(Mode::Run)));
    }

    #[test]
    fn console_flag_selects_console_mode() {
        assert!(matches!(
            parse_cli_args(args(&["--console"])),
            Ok(Mode::Console)
        ));
    }

    #[test]
    fn install_flag_selects_install_mode() {
        assert!(matches!(
            parse_cli_args(args(&["--install"])),
            Ok(Mode::Install)
        ));
    }

    #[test]
    fn uninstall_flag_selects_uninstall_mode() {
        assert!(matches!(
            parse_cli_args(args(&["--uninstall"])),
            Ok(Mode::Uninstall)
        ));
    }

    #[test]
    fn unknown_argument_is_error() {
        assert!(parse_cli_args(args(&["--verbose"])).is_err());
        assert!(
            parse_cli_args(args(&["console"])).is_err(),
            "только длинные флаги"
        );
    }

    #[test]
    fn extra_argument_is_error() {
        assert!(parse_cli_args(args(&["run", "extra"])).is_err());
        assert!(parse_cli_args(args(&["--console", "--install"])).is_err());
    }

    #[test]
    fn unknown_first_token_reported_before_extra_check() {
        // «bottle-svc bogus extra»: ругань на первый токен, а не на
        // «лишний аргумент extra».
        let error = parse_cli_args(args(&["bogus", "extra"])).unwrap_err();
        assert!(
            error.contains("неизвестный аргумент: bogus"),
            "ожидалась диагностика первого токена, получено: {error}"
        );
    }
}
