//! Миграция профилей 0.2.x (TOML-файлы `%LOCALAPPDATA%\BottleForApps\
//! bottles\*.toml` + `*.conf`) в зашифрованное хранилище (0.3, спека §4)
//! и полный сброс хранилища (настройки, «Сбросить хранилище»).
//!
//! Импорт — после разблокировки, одной кнопкой; битый файл пропускается
//! с причиной и не прерывает импорт (Review Focus #2); старые файлы
//! не удаляются (остаются бэкапом).

use std::fs;
use std::path::{Path, PathBuf};

use bottle_core::ipc::FullProfile;
use bottle_core::profile::{parse_profile, BottleKind};

use super::Vault;

/// Отчёт об импорте: что перенесено, что пропущено (id + причина).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Импортированные id (в порядке файлов).
    pub imported: Vec<String>,
    /// Пропущенные файлы: (имя файла, причина).
    pub skipped: Vec<(String, String)>,
}

/// Каталог профилей 0.2.x (`%LOCALAPPDATA%\BottleForApps\bottles`);
/// `None` вне Windows / без каталога данных.
pub fn legacy_dir() -> Option<PathBuf> {
    let base = dirs::data_local_dir()?;
    let dir = base.join("BottleForApps").join("bottles");
    dir.is_dir().then_some(dir)
}

/// Число TOML-профилей 0.2.x, доступных для импорта; `None` — каталога
/// нет, предлагать импорт не нужно.
// Вызывается Tauri-командой из Task 14+ (баннер «найдены профили 0.2.x»).
#[allow(dead_code)]
pub fn detect_legacy() -> Option<usize> {
    let dir = legacy_dir()?;
    Some(count_toml(&dir)).filter(|count| *count > 0)
}

/// Импортирует профили 0.2.x из штатного каталога в хранилище.
/// Бутылки с уже занятым id пропускаются (не затираем записи БД).
// Вызывается Tauri-командой из Task 14+ (кнопка «Импортировать»).
#[allow(dead_code)]
pub fn import_legacy(vault: &Vault) -> Result<ImportReport, String> {
    let dir = legacy_dir().ok_or_else(|| "профили 0.2.x не найдены".to_string())?;
    import_legacy_from(vault, &dir)
}

/// Импорт из явного каталога (тестовый шов и portable-разбор).
///
/// Каждый `*.toml` разбирается [`parse_profile`]; для env-бутылки текст
/// конфига читается из соседнего `<id>.conf`. Битый файл (разбор,
/// отсутствующий конфиг, дубль id в БД) попадает в `skipped` с причиной —
/// импорт остальных продолжается.
pub fn import_legacy_from(vault: &Vault, dir: &Path) -> Result<ImportReport, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("не удалось прочитать каталог {}: {e}", dir.display()))?;
    let mut tomls: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    tomls.sort();

    let mut report = ImportReport::default();
    for path in tomls {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match import_one(vault, &path) {
            Ok(id) => report.imported.push(id),
            Err(reason) => report.skipped.push((file, reason)),
        }
    }
    Ok(report)
}

/// Импорт одного файла профиля; причина отказа — строкой для отчёта.
fn import_one(vault: &Vault, path: &Path) -> Result<String, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("файл не читается: {e}"))?;
    let profile = parse_profile(&text).map_err(|e| format!("битый профиль: {e}"))?;
    let id = profile.id.clone();

    // Текст конфига env-бутылки — из соседнего `<id>.conf` (в 0.2.x поле
    // config нормализовано в имя файла копией в хранилище).
    let config_text = if profile.kind == BottleKind::Env {
        let conf = path.with_file_name(format!("{id}.conf"));
        Some(
            fs::read_to_string(&conf)
                .map_err(|e| format!("конфиг {} не читается: {e}", conf.display()))?,
        )
    } else {
        None
    };

    vault
        .create_bottle(&FullProfile {
            profile,
            config_text,
            credentials: None,
        })
        .map(|()| id)
}

/// Число `*.toml` в каталоге (без рекурсии).
fn count_toml(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|e| e.to_str()) == Some("toml")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Полный сброс хранилища (настройки): удаляет файл БД и спутники
/// WAL/SHM. Несуществующий файл — не ошибка (сброс идемпотентен).
/// Вызывается при ЗАКРЫТОМ хранилище (GUI закрывает Vault до вызова).
// Вызывается Tauri-командой настроек из Task 18 («Сбросить хранилище»).
#[allow(dead_code)]
pub fn reset_vault(path: &Path) -> Result<(), String> {
    for suffix in ["", "-wal", "-shm"] {
        let file = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            path.with_file_name(format!(
                "{}{suffix}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ))
        };
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("не удалось удалить {}: {e}", file.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог фикстур репозитория (tests/fixtures/legacy-bottles).
    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("legacy-bottles")
    }

    fn vault() -> (tempfile::TempDir, Vault) {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::create(&tmp.path().join(crate::db::VAULT_FILE_NAME), "pw").unwrap();
        (tmp, vault)
    }

    /// Импорт фикстур: office-a (env, с .conf) и corp-host перенесены,
    /// broken.toml пропущен с причиной, импорт не прервался (RF#2).
    /// Старые файлы остаются на месте.
    #[test]
    fn legacy_import_skips_broken_and_keeps_going() {
        let (_tmp, vault) = vault();
        let dir = fixtures();

        let report = import_legacy_from(&vault, &dir).unwrap();

        assert_eq!(report.imported, vec!["corp-host", "office-a"]);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "broken.toml");
        assert!(report.skipped[0].1.contains("битый профиль"));

        // env-профиль: конфиг переехал текстом из .conf фикстуры.
        let full = vault.get_full("office-a").unwrap().unwrap();
        let expected = fs::read_to_string(dir.join("office-a.conf")).unwrap();
        assert_eq!(full.config_text.as_deref(), Some(expected.as_str()));
        assert_eq!(full.profile.name, "Офис А");
        assert!(full.credentials.is_none(), "кредов в 0.2.x не было");

        // host-профиль: конфига нет.
        let host = vault.get_full("corp-host").unwrap().unwrap();
        assert!(host.config_text.is_none());

        // Старые файлы не удалены (бэкап).
        assert!(dir.join("office-a.toml").exists());
        assert!(dir.join("broken.toml").exists());
    }

    /// Повторный импорт: занятые id пропускаются, записи БД не затираются.
    #[test]
    fn reimport_skips_existing_ids() {
        let (_tmp, vault) = vault();
        let dir = fixtures();
        import_legacy_from(&vault, &dir).unwrap();

        // Пользователь изменил имя после первого импорта.
        let mut full = vault.get_full("office-a").unwrap().unwrap();
        full.profile.name = "Мой офис".to_string();
        vault.update_bottle(&full).unwrap();

        let report = import_legacy_from(&vault, &dir).unwrap();
        assert!(report.imported.is_empty(), "всё уже в БД");
        assert_eq!(report.skipped.len(), 3);
        assert!(report
            .skipped
            .iter()
            .any(|(f, reason)| f == "office-a.toml" && reason.contains("уже существует")));

        let after = vault.get_full("office-a").unwrap().unwrap();
        assert_eq!(after.profile.name, "Мой офис", "запись не затёрта");
    }

    /// env-профиль без соседнего .conf — пропуск с причиной.
    #[test]
    fn env_without_conf_is_skipped() {
        let (_tmp, vault) = vault();
        let tmp2 = tempfile::tempdir().unwrap();
        fs::copy(
            fixtures().join("office-a.toml"),
            tmp2.path().join("office-a.toml"),
        )
        .unwrap();

        let report = import_legacy_from(&vault, tmp2.path()).unwrap();
        assert!(report.imported.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].1.contains("конфиг"), "{}", report.skipped[0].1);
    }

    /// Сброс: удаляются БД и спутники WAL/SHM; повторный сброс — не
    /// ошибка.
    #[test]
    fn reset_vault_removes_db_and_wal_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::db::VAULT_FILE_NAME);
        let vault = Vault::create(&path, "pw").unwrap();
        drop(vault);
        // WAL-файл может быть уже слит checkpoint'ом — создаём явно,
        // чтобы проверить именно удаление спутников.
        fs::write(path.with_file_name("pogrebok.db-wal"), b"x").unwrap();
        fs::write(path.with_file_name("pogrebok.db-shm"), b"x").unwrap();

        reset_vault(&path).unwrap();
        assert!(!path.exists());
        assert!(!path.with_file_name("pogrebok.db-wal").exists());
        assert!(!path.with_file_name("pogrebok.db-shm").exists());

        reset_vault(&path).unwrap(); // идемпотентно
    }

    /// detect_legacy/count_toml: каталог с фикстурами даёт 3 toml.
    #[test]
    fn count_toml_counts_profiles() {
        assert_eq!(count_toml(&fixtures()), 3);
    }
}
