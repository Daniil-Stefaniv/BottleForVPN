//! Тонкий слой путей каталога данных демона (0.3). Дискового хранилища
//! профилей больше нет (профили живут в БД GUI и в памяти демона) —
//! на диске остаются только служебные файлы демона: `appliance-token`,
//! `generation-seed`, журналы `logs/`, каталог VM `vm/`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Открывает (создаёт при необходимости) каталог данных демона:
/// сам каталог и `logs/`. На unix каталог получает права 0700 — в нём
/// лежат секреты (токен аплаенса, спека §10); уже существующий каталог
/// с другими правами тоже ужесточается до 0700.
pub fn open_data_dir(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    fs::create_dir_all(logs_dir(root))?;
    set_private(root)?;
    Ok(())
}

/// Путь к файлу токена аплаенса (`<data>/appliance-token`).
pub fn appliance_token_path(root: &Path) -> PathBuf {
    root.join("appliance-token")
}

/// Каталог журналов демона (`<data>/logs`).
pub fn logs_dir(root: &Path) -> PathBuf {
    root.join("logs")
}

/// Права 0700 на unix (в каталоге — секреты); вне unix — no-op.
#[cfg(unix)]
fn set_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Права 0700 на unix (в каталоге — секреты); вне unix — no-op.
#[cfg(not(unix))]
fn set_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Атомарная запись: сначала временный файл рядом с целью, затем rename.
/// Существующий файл не портится при отказе на любом шаге; временный
/// файл при ошибке удаляется.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));

    let write = || -> io::Result<()> {
        let mut file = create_private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Создаёт файл с правами 0600 на unix (без окна с правами по umask);
/// на других платформах — обычное создание.
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Создаёт файл с правами 0600 на unix (без окна с правами по umask);
/// на других платформах — обычное создание.
#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_data_dir_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        open_data_dir(&root).unwrap();

        assert!(root.is_dir());
        assert!(logs_dir(&root).is_dir());
        assert_eq!(appliance_token_path(&root), root.join("appliance-token"));
    }

    #[cfg(unix)]
    #[test]
    fn open_data_dir_tightens_permissions_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        open_data_dir(&root).unwrap();

        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "каталог данных — только владелец");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token");
        atomic_write(&path, b"secret").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "secret");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "файл секрета — только владелец");
        assert!(
            !tmp.path().join("token.tmp").exists(),
            "временный файл убран"
        );
    }
}
