//! Файловые журналы службы: JSONL с ежедневной ротацией и хранением
//! 30 дней (спека «Диагностические журналы») в
//! `%PROGRAMDATA%\BottleForApps\logs` — процесс службы работает под
//! SYSTEM и не имеет консоли, файл — единственный надёжный сток
//! (stderr-слой сохраняется для `--console`).

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

/// Период фоновой чистки устаревших файлов (сутки: ротация ежедневная).
const PRUNE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Каталог журналов по умолчанию: `%PROGRAMDATA%\BottleForApps\logs`
/// (служба под SYSTEM); вне Windows — временный каталог (паттерн
/// [`crate::journal::default_journal_path`]).
pub fn default_logs_dir() -> PathBuf {
    match std::env::var_os("PROGRAMDATA") {
        Some(programdata) => Path::new(&programdata).join("BottleForApps").join("logs"),
        None => std::env::temp_dir().join("BottleForApps").join("logs"),
    }
}

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
/// ротацией (`<base>.YYYY-MM-DD`) + stderr-слой (виден только в
/// `--console`); уровень — `RUST_LOG` (по умолчанию `info`). Фоновый
/// поток раз в сутки удаляет файлы своего префикса старше
/// [`RETENTION`] (по mtime, без парсинга).
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
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(appender),
        )
        .init();

    let prune_dir = dir.to_path_buf();
    let prune_base = base.to_string();
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

    /// Сегодняшняя дата `YYYY-MM-DD` без chrono (алгоритм Хиннанта).
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

    /// Ретеншн: только файлы своего префикса старше 30 дней удаляются;
    /// свежие (29 дней) и чужие файлы каталога не трогаются.
    #[test]
    fn prune_deletes_only_this_base_files_older_than_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let old = file_with_age(tmp.path(), "bottle-svc.2025-01-01", DAY * 31);
        let fresh = file_with_age(tmp.path(), "bottle-svc.today", DAY * 29);
        let foreign = file_with_age(tmp.path(), "routes.jsonl", DAY * 31);

        let removed = prune_old_logs(tmp.path(), "bottle-svc", RETENTION, SystemTime::now());

        assert_eq!(removed, 1, "удалён ровно один устаревший файл");
        assert!(!old.exists(), "31 день — удалён");
        assert!(fresh.exists(), "29 дней — жив");
        assert!(
            foreign.exists(),
            "журнал маршрутов — не файл трейсинга, не трогаем"
        );
    }

    /// Единственный тест бинарника, инициализирующий глобальный
    /// подписчик (cargo-тесты крейта живут в одном процессе): после
    /// init событие попадает в файл с сегодняшним именем.
    #[test]
    fn init_creates_today_file_and_writes_events() {
        let tmp = tempfile::tempdir().unwrap();
        init_file_logging(tmp.path(), "bottle-svc").expect("инициализация журнала");

        tracing::info!("тестовая запись журнала");

        let expected = tmp.path().join(format!("bottle-svc.{}", today()));
        let content = fs::read_to_string(&expected).unwrap_or_else(|error| {
            panic!("файл {} существует и читается: {error}", expected.display())
        });
        assert!(
            content.contains("тестовая запись журнала"),
            "событие попало в файл: {content}"
        );
    }

    /// Каталог по умолчанию повторяет прецедент журнала маршрутов:
    /// PROGRAMDATA → каталог данных, иначе временный каталог (dev);
    /// точка монтирования — подкаталог `BottleForApps/logs`.
    #[test]
    fn default_logs_dir_mounts_logs_subdirectory() {
        assert!(default_logs_dir().ends_with(Path::new("BottleForApps").join("logs")));
    }
}
