//! Сайдкар-мост к CLI `bottle` (рулинг D1): GUI не дублирует бизнес-логику,
//! а спавнит подпроцесс `bottle --json …` и отдаёт его stdout/код выхода
//! в TypeScript. Таймаут 130 с покрывает `start` (холодный старт аплаенса
//! + handshake туннеля) с запасом над CLI-ожиданием 120 с.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// Переменная окружения для подмены пути к CLI в dev-режиме и тестах.
pub const CLI_BIN_ENV: &str = "POGREBOK_GUI_CLI_BIN";

/// Бюджет на один вызов CLI: `start` в худшем случае занимает до 120 с
/// (CLI-транспорт сам ждёт столько же) — GUI не отрезает его раньше.
pub const BOTTLE_TIMEOUT: Duration = Duration::from_secs(130);

/// `CREATE_NO_WINDOW` (Windows): GUI — windows-подсистема без консоли,
/// поэтому каждый спавн консольного `pogrebok.exe`/`pogrebok-daemon.exe`
/// иначе создаёт видимое окно cmd (при периодическом refresh — «моргание»).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Результат вызова CLI для TypeScript-стороны.
#[derive(Debug, Clone, Serialize)]
pub struct BottleOutput {
    /// stdout подпроцесса (в json-режиме — CliReply одной строкой).
    pub stdout: String,
    /// Код выхода (0 ок / 1 ошибка / 2 парсинг).
    pub code: i32,
}

/// Резолвит исполняемый файл CLI: переменная окружения (dev/тесты) →
/// `bottle` рядом с GUI (MSI/.app, рулинг D5) → поиск в PATH.
pub fn resolve_bottle_path() -> PathBuf {
    if let Ok(path) = std::env::var(CLI_BIN_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let candidate = dir.join(bottle_binary_name());
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(bottle_binary_name())
}

/// Имя файла CLI на текущей платформе.
fn bottle_binary_name() -> &'static str {
    if cfg!(windows) {
        "bottle.exe"
    } else {
        "bottle"
    }
}

/// Выполняет `bottle <args>`, возвращает stdout и код выхода.
pub async fn run_bottle(args: &[String]) -> Result<BottleOutput, String> {
    run_program(resolve_bottle_path(), args.to_vec(), BOTTLE_TIMEOUT).await
}

/// Спавнит программу, собирает stdout (UTF-8, lossy) и код выхода;
/// по таймауту убивает процесс и возвращает ошибку на русском.
pub async fn run_program(
    program: impl Into<PathBuf>,
    args: Vec<String>,
    timeout: Duration,
) -> Result<BottleOutput, String> {
    let program = program.into();
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Без CREATE_NO_WINDOW спавн консольного подпроцесса из GUI
    // показывал бы окно cmd на каждый вызов.
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let child = command
        .spawn()
        .map_err(|error| format!("не удалось запустить {}: {error}", program.display()))?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(BottleOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            code: output.status.code().unwrap_or(-1),
        }),
        Ok(Err(error)) => Err(format!("ошибка ожидания {}: {error}", program.display())),
        Err(_) => Err(format!(
            "процесс {} не ответил за {} с",
            program.display(),
            timeout.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кроссплатформенный «эхо» для тестов спавна.
    fn echo_program() -> (&'static str, Vec<String>) {
        if cfg!(windows) {
            ("cmd", vec!["/C".into(), "echo hi".into()])
        } else {
            ("/bin/echo", vec!["hi".into()])
        }
    }

    /// Кроссплатформенный «сон» для теста таймаута.
    fn sleep_program() -> (&'static str, Vec<String>) {
        if cfg!(windows) {
            ("ping", vec!["-n".into(), "10".into(), "127.0.0.1".into()])
        } else {
            ("/bin/sleep", vec!["10".into()])
        }
    }

    #[tokio::test]
    async fn run_program_captures_stdout_and_exit_code() {
        let (program, args) = echo_program();
        let output = run_program(program, args, BOTTLE_TIMEOUT)
            .await
            .expect("эхо выполняется");
        assert_eq!(output.code, 0);
        assert!(
            output.stdout.trim_end().ends_with("hi"),
            "{}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn run_program_reports_nonzero_exit_code() {
        if cfg!(windows) {
            // cmd /C exit 3
            let output = run_program("cmd", vec!["/C".into(), "exit 3".into()], BOTTLE_TIMEOUT)
                .await
                .expect("cmd выполняется");
            assert_eq!(output.code, 3);
        } else {
            let output = run_program(
                "/bin/sh",
                vec!["-c".into(), "exit 3".into()],
                BOTTLE_TIMEOUT,
            )
            .await
            .expect("sh выполняется");
            assert_eq!(output.code, 3);
        }
    }

    #[tokio::test]
    async fn run_program_kills_on_timeout() {
        let (program, args) = sleep_program();
        let started = std::time::Instant::now();
        let result = run_program(program, args, std::time::Duration::from_millis(300)).await;
        let error = result.expect_err("десятисекундный сон при таймауте 300 мс убивается");
        assert!(
            error.contains("не ответил за"),
            "сообщение о таймауте: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "процесс убит, а не дожидается сна"
        );
    }

    #[tokio::test]
    async fn run_program_missing_binary_is_error() {
        let result = run_program(
            "pogrebok-gui-no-such-binary-xyz",
            vec![],
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(result.is_err());
    }

    /// Переменная окружения переопределяет путь к bottle (dev/тесты);
    /// выставляется одной командой с файловой переменной, чтобы тесты
    /// одного бинарника не гонялись за ней.
    #[test]
    fn resolve_bottle_path_honors_env_override() {
        // SAFETY: std::env::set_var безопасен в однопоточном тестовом
        // рантайме; переменная снимается до возврата.
        unsafe {
            std::env::set_var(CLI_BIN_ENV, "/opt/bottle-test");
        }
        assert_eq!(
            resolve_bottle_path(),
            std::path::PathBuf::from("/opt/bottle-test")
        );
        unsafe {
            std::env::remove_var(CLI_BIN_ENV);
        }
    }
}
