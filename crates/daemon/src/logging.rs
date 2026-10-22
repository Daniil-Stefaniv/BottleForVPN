//! Файловые журналы демона: JSONL с ежедневной ротацией и хранением
//! 30 дней (спека «Диагностические журналы»), маскирование аргументов
//! VPN-клиентов (спека §10).
//!
//! Каталог — `<каталог данных>/logs`; stdout-слой сохраняется (демон
//! работает в сессии пользователя, консоль полезна). Паттерн дублируется
//! в `svc/src/logging.rs` без общей зависимости: у крейтов разные наборы
//! слоёв (прецедент hex/ps_value).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Срок хранения журналов (спека «Диагностические журналы»).
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Маска аргументов запуска VPN-клиентов (спека §10): строка
/// скрывается целиком — адреса, логины и пути не разбираются.
pub const MASKED_ARGS: &str = "<аргументы скрыты>";

/// Период фоновой чистки устаревших файлов (сутки: ротация ежедневная,
/// чистка с той же периодичностью держит хранение ровно 30 дней).
const PRUNE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Ошибка инициализации файлового журнала.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// Не удалось создать каталог журналов.
    #[error("не удалось создать каталог журналов {dir}: {source}")]
    CreateDir {
        /// Каталог, который не удалось создать.
        dir: PathBuf,
        /// Ошибка файловой системы.
        source: io::Error,
    },
    /// Не удалось открыть файл журнала (права, диск).
    #[error("не удалось открыть файл журнала в {dir}: {source}")]
    OpenFile {
        /// Каталог журналов.
        dir: PathBuf,
        /// Ошибка построения appender'а.
        source: InitError,
    },
}

/// Инициализирует файловое логирование в `dir`: JSONL-слой с ежедневной
/// ротацией (`<base>.YYYY-MM-DD`) + сохраняющийся stdout-слой; уровень —
/// `RUST_LOG` (по умолчанию `info`). Фоновый поток раз в сутки удаляет
/// файлы своего префикса старше [`RETENTION`] (по mtime, без парсинга).
///
/// Ошибка не фатальна для вызывающего: демон продолжает работу со
/// stdout-логами (деградация наблюдаемости, не функции).
pub fn init_file_logging(dir: &Path, base: &str) -> Result<(), LoggingError> {
    fs::create_dir_all(dir).map_err(|source| LoggingError::CreateDir {
        dir: dir.to_path_buf(),
        source,
    })?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        // Разделитель «.» между префиксом и датой добавляет сам appender.
        .filename_prefix(base)
        .build(dir)
        .map_err(|source| LoggingError::OpenFile {
            dir: dir.to_path_buf(),
            source,
        })?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(appender),
        )
        .init();

    let prune_dir = dir.to_path_buf();
    let prune_base = base.to_string();
    // Чистка — фон: замедленный старт журнала не задерживает демон.
    let _ = thread::Builder::new()
        .name("log-prune".to_string())
        .spawn(move || loop {
            let removed = prune_old_logs(&prune_dir, &prune_base, RETENTION, SystemTime::now());
            if removed > 0 {
                tracing::info!(removed, "устаревшие файлы журналов удалены");
            }
            thread::sleep(PRUNE_PERIOD);
        });
    Ok(())
}

/// Маскирует аргументы запуска VPN-клиента для журнала: непустая
/// строка целиком заменяется плейсхолдером, пустая остаётся пустой
/// (нечего скрывать — и не создавать шум плейсхолдерами).
pub fn mask_args(args: &str) -> String {
    if args.is_empty() {
        String::new()
    } else {
        MASKED_ARGS.to_string()
    }
}

// ---------------------------------------------------------------------
// Событийный журнал 0.3 (спека §6): структурные события Event из
// реестра bottle_core::events — JSONL-файл `<logs>/events.jsonl` и
// push-поток DaemonEvent::Log подписчикам (GUI «Диагностика»).
// ---------------------------------------------------------------------

/// Файл событийного журнала (`events.jsonl` в каталоге журналов):
/// append-only, одна JSONL-строка на событие.
pub struct EventLog {
    file: fs::File,
}

impl EventLog {
    /// Открывает (создаёт) `events.jsonl` в `logs_dir` на дозапись.
    /// На unix файл — 0600 (в журнале нет секретов, но есть топология).
    pub fn open(logs_dir: &Path) -> io::Result<EventLog> {
        fs::create_dir_all(logs_dir)?;
        let path = logs_dir.join("events.jsonl");
        let file = open_private_append(&path)?;
        Ok(EventLog { file })
    }

    /// Дописывает событие одной JSONL-строкой.
    pub fn write(&mut self, event: &bottle_core::events::Event) -> io::Result<()> {
        use std::io::Write;
        let line = serde_json::to_string(event).map_err(io::Error::other)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")
    }
}

/// Append-открытие с правами 0600 на unix.
#[cfg(unix)]
fn open_private_append(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Append-открытие с правами 0600 на unix.
#[cfg(not(unix))]
fn open_private_append(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Разделяемый файловый журнал событий демона (None — журнал недоступен,
/// события идут только подписчикам).
pub type SharedEventLog = std::sync::Arc<std::sync::Mutex<Option<EventLog>>>;

/// Пишет событие в файловый журнал и шлёт `DaemonEvent::Log`
/// подписчикам (один фаннел для демона и менеджера бутылок).
///
/// ctx санитизируется [`bottle_core::events::sanitize_ctx`]: в запись
/// уходят только ключи шаблона (секреты в журнал не попадают). Код вне
/// реестра — debug-запись «вне реестра» с пустым контекстом: потерять
/// факт события хуже, чем потерять его формулировку.
pub fn emit(
    events: &tokio::sync::broadcast::Sender<bottle_core::ipc::DaemonEvent>,
    log: &SharedEventLog,
    src: bottle_core::events::Src,
    code: &str,
    ctx: serde_json::Map<String, serde_json::Value>,
) {
    use bottle_core::events::Level;

    let ctx = bottle_core::events::sanitize_ctx(code, ctx);
    let (level, desc) = match bottle_core::events::render(code, ctx.clone()) {
        Some((level, desc)) => (level, desc),
        None => {
            tracing::debug!(code, "событие вне реестра");
            (Level::Debug, format!("{code} (вне реестра событий)"))
        }
    };
    let event = bottle_core::events::Event {
        ts: bottle_core::events::now_ts(),
        level,
        src,
        event: code.to_string(),
        desc,
        ctx,
    };
    if let Ok(mut guard) = log.lock() {
        if let Some(log) = guard.as_mut() {
            if let Err(error) = log.write(&event) {
                tracing::warn!(%error, "событийный журнал недоступен");
                // Дальше файл не трогаем: повторные сбои не спамят.
                *guard = None;
            }
        }
    }
    let _ = events.send(bottle_core::ipc::DaemonEvent::Log(Box::new(event)));
}

/// Удаляет из `dir` файлы с префиксом `<base>.` старше `retention`
/// (по mtime). Возвращает число удалённых; ошибки отдельных файлов
/// пропускаются (чистка — фон, следующий проход повторит).
pub fn prune_old_logs(dir: &Path, base: &str, retention: Duration, now: SystemTime) -> usize {
    let prefix = format!("{base}.");
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        let Some(modified) = modified else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > retention)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// Создаёт в `dir` файл `name` с mtime «сейчас − age».
    fn file_with_age(dir: &Path, name: &str, age: Duration) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, "x").unwrap();
        let mtime = filetime::FileTime::from_system_time(SystemTime::now() - age);
        filetime::set_file_mtime(&path, mtime).unwrap();
        path
    }

    /// Сегодняшняя дата `YYYY-MM-DD` без chrono: дни unix-эпохи в
    /// григорианскую дату (алгоритм Г. Хиннанта) — чтобы ожидать в
    /// имени файла ротации сегодняшний день.
    fn today() -> String {
        let days = (SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 86_400) as i64;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// Аргументы клиентов маскируются целиком (спека §10): в строке
    /// могут быть адреса, логины, пути — скрыть всё проще и надёжнее
    /// разбора по флагам.
    #[test]
    fn mask_args_hides_any_nonempty_argument_line() {
        assert_eq!(mask_args("connect office --user admin"), MASKED_ARGS);
        assert_eq!(mask_args("x"), MASKED_ARGS);
        assert_eq!(mask_args(""), "", "пустые аргументы — нечего скрывать");
    }

    /// Ретеншн: только файлы своего префикса старше 30 дней удаляются;
    /// свежие (29 дней) и чужие файлы каталога не трогаются.
    #[test]
    fn prune_deletes_only_this_base_files_older_than_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let old = file_with_age(tmp.path(), "bottled.2025-01-01", DAY * 31);
        let fresh = file_with_age(tmp.path(), "bottled.today", DAY * 29);
        let foreign = file_with_age(tmp.path(), "other.2025-01-01", DAY * 31);

        let removed = prune_old_logs(tmp.path(), "bottled", RETENTION, SystemTime::now());

        assert_eq!(removed, 1, "удалён ровно один устаревший файл");
        assert!(!old.exists(), "31 день — удалён");
        assert!(fresh.exists(), "29 дней — жив");
        assert!(foreign.exists(), "чужие файлы каталога не трогаем");
    }

    /// Событийный журнал: emit пишет JSONL-строку в events.jsonl и шлёт
    /// то же событие подписчикам broadcast (push в GUI).
    #[test]
    fn emit_writes_event_and_broadcasts() {
        let tmp = tempfile::tempdir().unwrap();
        let log: SharedEventLog =
            std::sync::Arc::new(std::sync::Mutex::new(Some(EventLog::open(tmp.path()).unwrap())));
        let (events, mut rx) = tokio::sync::broadcast::channel(8);

        let mut ctx = serde_json::Map::new();
        ctx.insert("id".into(), "office-a".into());
        ctx.insert("port".into(), 1080.into());
        emit(
            &events,
            &log,
            bottle_core::events::Src::Daemon,
            "bottle.up",
            ctx,
        );

        let line = fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        assert!(line.contains(r#""event":"bottle.up""#), "код в файле: {line}");
        assert!(line.contains(r#""port":1080"#), "порт в ctx: {line}");
        assert!(line.contains("office-a"), "описание в файле: {line}");

        let pushed = rx.try_recv().expect("событие ушло подписчикам");
        let bottle_core::ipc::DaemonEvent::Log(event) = pushed else {
            panic!("ожидали Log, получили {pushed:?}");
        };
        assert_eq!(event.event, "bottle.up");
        assert_eq!(event.desc, "office-a поднята (socks5 :1080)");
    }

    /// Сканер секретов: ctx с password/PrivateKey не сериализуется —
    /// в запись уходят только ключи шаблона (спека §6).
    #[test]
    fn log_line_never_contains_secret_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let log: SharedEventLog =
            std::sync::Arc::new(std::sync::Mutex::new(Some(EventLog::open(tmp.path()).unwrap())));
        let (events, _rx) = tokio::sync::broadcast::channel::<bottle_core::ipc::DaemonEvent>(8);

        let mut ctx = serde_json::Map::new();
        ctx.insert("id".into(), "office-a".into());
        ctx.insert("port".into(), 1080.into());
        ctx.insert("password".into(), "s3cr3t-P@ss".into());
        ctx.insert("PrivateKey".into(), "PRIVATE-KEY-MATERIAL".into());
        emit(
            &events,
            &log,
            bottle_core::events::Src::Daemon,
            "bottle.up",
            ctx,
        );
        // Код вне реестра: контекст отбрасывается целиком.
        let mut ctx = serde_json::Map::new();
        ctx.insert("token".into(), "TOP-SECRET".into());
        emit(
            &events,
            &log,
            bottle_core::events::Src::Daemon,
            "custom.unknown",
            ctx,
        );

        let content = fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        for secret in ["s3cr3t-P@ss", "PRIVATE-KEY-MATERIAL", "TOP-SECRET"] {
            assert!(
                !content.contains(secret),
                "секрет утёк в журнал: {secret}\n{content}"
            );
        }
        for key in ["password", "PrivateKey"] {
            assert!(
                !content.contains(key),
                "имя секретного ключа не пишется: {key}\n{content}"
            );
        }
        assert!(
            content.contains("custom.unknown (вне реестра событий)"),
            "неизвестный код — debug-fallback: {content}"
        );
    }

    /// Единственный тест бинарника, инициализирующий глобальный
    /// подписчик (cargo-тесты крейта живут в одном процессе):
    /// после init событие попадает в файл с сегодняшним именем.
    #[test]
    fn init_creates_today_file_and_writes_events() {
        let tmp = tempfile::tempdir().unwrap();
        init_file_logging(tmp.path(), "bottled").expect("инициализация журнала");

        tracing::info!("тестовая запись журнала");

        let expected = tmp.path().join(format!("bottled.{}", today()));
        let content = fs::read_to_string(&expected).unwrap_or_else(|error| {
            panic!("файл {} существует и читается: {error}", expected.display())
        });
        assert!(
            content.contains("тестовая запись журнала"),
            "событие попало в файл: {content}"
        );
    }
}
