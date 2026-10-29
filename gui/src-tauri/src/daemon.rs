//! Спавн демона `pogrebok-daemon` из GUI (рулинг D1, единственное место,
//! где GUI запускает не-CLI): кнопка «Запустить демона» поднимает демона
//! и показывает его журнал в окне. Демон переживёт выход GUI —
//! kill_on_drop НЕ ставим (Task 5: «GUI выходит; демон живёт»).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::process::{Child, Command};

/// Переменная окружения для подмены пути к демону в dev-режиме и тестах.
pub const DAEMON_BIN_ENV: &str = "POGREBOK_GUI_DAEMON_BIN";

/// Хвост журнала демона, который держит GUI (кольцо ~32 КиБ).
const LOG_LIMIT: usize = 32 * 1024;

/// `CREATE_NO_WINDOW` (Windows): GUI — windows-подсистема без консоли,
/// спавн консольного `pogrebok-daemon.exe` иначе создаёт видимое окно cmd.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
/// за Arc — ридер живёт в 'static-задаче). Токен авторизации
/// генерируется при старте GUI и передаётся демону `--gui-token`
/// (0.3, спека §5; живёт только в памяти GUI).
#[derive(Clone)]
pub struct DaemonHandle {
    inner: Arc<Mutex<DaemonInner>>,
    token: String,
}

impl Default for DaemonHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DaemonInner::default())),
            token: crate::daemon_client::generate_token(),
        }
    }
}

impl DaemonHandle {
    /// Токен авторизации GUI→демон этой сессии (для DaemonClient).
    // Проводится в Tauri-команды в задачах 14–15 (запуск демона + sync).
    #[allow(dead_code)]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Запускает демона `pogrebok-daemon` (резолв как у CLI: env → рядом
    /// с exe → PATH) с токеном авторизации; повторный вызов при живом
    /// процессе — ошибка.
    pub async fn start(&self) -> Result<(), String> {
        self.start_custom(
            resolve_daemon_path(),
            vec!["--gui-token".to_string(), self.token.clone()],
        )
        .await
    }

    /// Запускает конкретную программу как «демона» (тесты/dev).
    pub async fn start_custom(
        &self,
        program: impl Into<PathBuf>,
        args: Vec<String>,
    ) -> Result<(), String> {
        let program = program.into();
        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Без CREATE_NO_WINDOW старт консольного pogrebok-daemon из GUI
        // показывал бы окно cmd.
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command
            // Демон живёт дольше GUI: kill_on_drop сознательно НЕ ставим.
            .spawn()
            .map_err(|error| format!("не удалось запустить {}: {error}", program.display()))?;

        // Один процесс на окно: при живом демоне повторный старт — ошибка.
        // Проверка «уже запущен» — в своём scope: гвард не живёт в await.
        let already_running = {
            let mut inner = self.lock();
            match inner.child.as_mut() {
                Some(existing) => matches!(existing.try_wait(), Ok(None)),
                None => false,
            }
        };
        if already_running {
            // Дубль не бросаем: без kill_on_drop дроп = detach,
            // осиротевший второй демон пережил бы отказ (I1).
            // Короткая грация: дать дублю начать запись лога/файлов
            // старта (старт демона длинный, 150 мс роли не играют).
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("демон уже запущен этим окном".to_string());
        }
        {
            let mut inner = self.lock();
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
            // Сдвигаем cut до границы UTF-8-символа (журнал — русский):
            // drain по негранице паникует.
            let mut cut = inner.log.len() - LOG_LIMIT;
            while !inner.log.is_char_boundary(cut) {
                cut += 1;
            }
            inner.log.drain(..cut);
        }
    }
}

/// Резолвит исполняемый файл демона: env (dev/тесты) → рядом с GUI
/// (MSI/.app, D5) → PATH.
fn resolve_daemon_path() -> PathBuf {
    if let Ok(path) = std::env::var(DAEMON_BIN_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let candidate = dir.join(daemon_binary_name());
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(daemon_binary_name())
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "pogrebok-daemon.exe"
    } else {
        "pogrebok-daemon"
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

    /// I1 ревью: повторный старт при живом демоне обязан убить только
    /// что заспавненный дубль (без kill_on_drop дроп = detach).
    #[cfg(unix)]
    #[tokio::test]
    async fn second_start_kills_spawned_duplicate() {
        let dir = tempfile::TempDir::new().unwrap();
        let program = "/bin/sh".to_string();
        let args_for = |name: &str| {
            vec![
                "-c".to_string(),
                format!("echo $$ > {}; sleep 30", dir.path().join(name).display()),
            ]
        };

        let daemon = DaemonHandle::default();
        daemon
            .start_custom(program.clone(), args_for("pid1"))
            .await
            .expect("первый старт");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Дубль пишет СВОЙ pid (pid2) — его и проверяем.
        let error = daemon
            .start_custom(program, args_for("pid2"))
            .await
            .expect_err("повторный старт отказывает");
        assert!(error.contains("уже запущен"), "{error}");

        // Дубль обязан быть мёртв (kill в error-ветке, а не detach).
        let pid_file2 = dir.path().join("pid2");
        let mut pid: Option<i32> = None;
        for _ in 0..30 {
            if let Ok(text) = std::fs::read_to_string(&pid_file2) {
                pid = text.trim().parse().ok();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let pid: i32 = pid.expect("pid дубля записан (грация старта)");
        let mut dead = false;
        for _ in 0..40 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        daemon.stop().await.expect("cleanup");
        assert!(dead, "дубль (pid {pid}) пережил отказ «уже запущен»");
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

    /// I3 ревью: обрезка кольцевого буфера обязана попадать в границу
    /// UTF-8-символа (русский журнал демона) — drain(..cut) паникует иначе.
    #[test]
    fn log_tail_trim_respects_char_boundary() {
        let daemon = DaemonHandle::default();
        let log = LogProbe::capture(&daemon);
        // Кириллица (2 байта/символ) вперемешку с одиночными байтами:
        // cut обязан попадать внутрь многобайтного символа — drain
        // без поправки границы паникует именно там.
        let chunk = "проверка-журнала-".repeat(64);
        for i in 0..700 {
            log.append(&daemon, &chunk);
            if i % 2 == 0 {
                log.append(&daemon, "x");
            }
        }
        let after = log.len(&daemon);
        assert!(
            after <= LOG_LIMIT + 64,
            "буфер обрезан около лимита, а не растёт бесконечно: {after}"
        );
    }

    /// Доступ к внутренностям журнала для теста обрезки.
    struct LogProbe;

    impl LogProbe {
        fn capture(_daemon: &DaemonHandle) -> Self {
            LogProbe
        }

        fn len(&self, daemon: &DaemonHandle) -> usize {
            daemon.inner.lock().unwrap().log.len()
        }

        fn append(&self, daemon: &DaemonHandle, chunk: &str) {
            let handle = LogHandle {
                inner: Arc::clone(&daemon.inner),
            };
            handle.append(chunk);
        }
    }

    #[tokio::test]
    async fn missing_daemon_binary_is_error() {
        let daemon = DaemonHandle::default();
        let error = daemon
            .start_custom("pogrebok-daemon-no-such-binary-xyz", vec![])
            .await
            .expect_err("отсутствующий бинарь — ошибка");
        assert!(error.contains("не удалось запустить"), "{error}");
    }
}
