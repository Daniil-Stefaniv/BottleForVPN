//! Спавн демона `bottled` из GUI (рулинг D1, единственное место, где GUI
//! запускает не-CLI): кнопка «Запустить демона» поднимает bottled и
//! показывает его журнал в окне. Демон переживёт выход GUI — kill_on_drop
//! НЕ ставим (Task 5: «GUI выходит; демон живёт»).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::process::{Child, Command};

/// Переменная окружения для подмены пути к демону в dev-режиме и тестах.
pub const BOTTLED_BIN_ENV: &str = "BOTTLE_GUI_BOTTLED_BIN";

/// Хвост журнала демона, который держит GUI (кольцо ~32 КиБ).
const LOG_LIMIT: usize = 32 * 1024;

/// Статус демона для фронтенда.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    /// Запущен ли процесс сейчас.
    pub running: bool,
    /// Хвост stdout+stderr демона.
    pub log_tail: String,
}

#[derive(Default)]
struct DaemonInner {
    child: Option<Child>,
    log: String,
}

/// Управление спавненным демоном: один экземпляр на приложение
/// (tauri State), журнал дочитывается фоновой задачей (внутренность
/// за Arc — ридер живёт в 'static-задаче).
#[derive(Default, Clone)]
pub struct DaemonHandle {
    inner: Arc<Mutex<DaemonInner>>,
}

impl DaemonHandle {
    /// Запускает демона `bottled` (резолв как у CLI: env → рядом с exe →
    /// PATH); повторный вызов при живом процессе — ошибка.
    pub async fn start(&self) -> Result<(), String> {
        self.start_custom(resolve_bottled_path(), vec![]).await
    }

    /// Запускает конкретную программу как «демона» (тесты/dev).
    pub async fn start_custom(
        &self,
        program: impl Into<PathBuf>,
        args: Vec<String>,
    ) -> Result<(), String> {
        let program = program.into();
        let child = Command::new(&program)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Демон живёт дольше GUI: kill_on_drop сознательно НЕ ставим.
            .spawn()
            .map_err(|error| format!("не удалось запустить {}: {error}", program.display()))?;

        // Один процесс на окно: при живом демоне повторный старт — ошибка.
        {
            let mut inner = self.lock();
            let already_running = match inner.child.as_mut() {
                Some(existing) => matches!(existing.try_wait(), Ok(None)),
                None => false,
            };
            if already_running {
                return Err("демон уже запущен этим окном".to_string());
            }
            inner.child = Some(child);
            inner.log.clear();

            // Ридер журнала: обе трубы в общий буфер до конца процесса.
            let current = inner.child.as_mut().expect("только что записали");
            let stdout = current.stdout.take();
            let stderr = current.stderr.take();
            let log = self.log_handle();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let streams: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = [
                    stdout.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                    stderr.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                ]
                .into_iter()
                .flatten()
                .collect();
                for mut stream in streams {
                    let mut buffer = [0u8; 2048];
                    loop {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => log.append(&String::from_utf8_lossy(&buffer[..n])),
                        }
                    }
                }
            });
        }
        Ok(())
    }

    /// Текущий статус: жив ли процесс + хвост журнала.
    pub fn status(&self) -> DaemonStatus {
        let mut inner = self.lock();
        let running = match inner.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) | Err(_) => false,
                Ok(None) => true,
            },
            None => false,
        };
        DaemonStatus {
            running,
            log_tail: inner.log.clone(),
        }
    }

    /// Останавливает демона (выход GUI это НЕ вызывает; для тестов и
    /// явного админ-действия).
    // Используется тестами; кнопки остановки демона в интерфейсе нет
    // (осознанно, D4).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn stop(&self) -> Result<(), String> {
        // Гварда не держим через await (clippy::await_holding_lock):
        // забираем child из состояния, kill'им вне мьютекса.
        let child = {
            let mut inner = self.lock();
            inner.child.take()
        };
        match child {
            Some(mut child) => child
                .kill()
                .await
                .map_err(|error| format!("не удалось остановить демона: {error}")),
            None => Err("демон не запущен этим окном".to_string()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DaemonInner> {
        self.inner.lock().expect("мьютекс демона не отравлен")
    }

    fn log_handle(&self) -> LogHandle {
        LogHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Фоновая запись в журнал из ридер-задачи ('static: держит Arc).
struct LogHandle {
    inner: Arc<Mutex<DaemonInner>>,
}

impl LogHandle {
    fn append(&self, chunk: &str) {
        let mut inner = self.inner.lock().expect("мьютекс демона не отравлен");
        inner.log.push_str(chunk);
        if inner.log.len() > LOG_LIMIT {
            let cut = inner.log.len() - LOG_LIMIT;
            inner.log.drain(..cut);
        }
    }
}

/// Резолвит исполняемый файл демона: env (dev/тесты) → рядом с GUI
/// (MSI/.app, D5) → PATH.
fn resolve_bottled_path() -> PathBuf {
    if let Ok(path) = std::env::var(BOTTLED_BIN_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let candidate = dir.join(bottled_binary_name());
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(bottled_binary_name())
}

fn bottled_binary_name() -> &'static str {
    if cfg!(windows) {
        "bottled.exe"
    } else {
        "bottled"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кроссплатформенный «демон»: печатает строку и живёт.
    fn daemon_program() -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd".into(),
                vec![
                    "/C".into(),
                    "echo started & ping -n 30 127.0.0.1 > NUL".into(),
                ],
            )
        } else {
            (
                "/bin/sh".into(),
                vec!["-c".into(), "echo started; sleep 30".into()],
            )
        }
    }

    #[tokio::test]
    async fn start_daemon_reports_running_and_log() {
        let daemon = DaemonHandle::default();
        let (program, args) = daemon_program();
        daemon
            .start_custom(program, args)
            .await
            .expect("демон-дубль стартует");
        // Дать процессу время напечатать строку и её прочитать.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let status = daemon.status();
        assert!(status.running, "долгоживущий процесс жив");
        assert!(
            status.log_tail.contains("started"),
            "лог: {}",
            status.log_tail
        );
        daemon.stop().await.expect("тестовый стоп работает");
    }

    #[tokio::test]
    async fn second_start_while_running_is_error() {
        let daemon = DaemonHandle::default();
        let (program, args) = daemon_program();
        daemon
            .start_custom(program.clone(), args.clone())
            .await
            .expect("первый старт");
        let error = daemon
            .start_custom(program, args)
            .await
            .expect_err("повторный старт отказывает");
        assert!(error.contains("уже запущен"), "{error}");
        daemon.stop().await.expect("стоп");
    }

    #[tokio::test]
    async fn status_after_exit_reports_stopped_with_tail() {
        let daemon = DaemonHandle::default();
        if cfg!(windows) {
            daemon
                .start_custom("cmd", vec!["/C".to_string(), "echo bye".to_string()])
                .await
                .expect("короткий процесс");
        } else {
            daemon
                .start_custom("/bin/echo", vec!["bye".to_string()])
                .await
                .expect("короткий процесс");
        }
        // Ждём, пока процесс завершится и ридер дочитает.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let status = daemon.status();
        assert!(!status.running, "завершившийся процесс не «жив»");
        assert!(status.log_tail.contains("bye"), "лог: {}", status.log_tail);
    }

    #[tokio::test]
    async fn stop_without_daemon_is_error() {
        let daemon = DaemonHandle::default();
        assert!(daemon.stop().await.is_err());
    }

    #[tokio::test]
    async fn missing_bottled_binary_is_error() {
        let daemon = DaemonHandle::default();
        let error = daemon
            .start_custom("bottled-no-such-binary-xyz", vec![])
            .await
            .expect_err("отсутствующий бинарь — ошибка");
        assert!(error.contains("не удалось запустить"), "{error}");
    }
}
