//! Токен авторизации control-канала: создание и чтение файла.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

/// Источник энтропии для нового токена.
const RANDOM_SOURCE: &str = "/dev/urandom";

/// Размер токена в байтах до hex-кодирования (hex — 64 символа).
const TOKEN_BYTES: usize = 32;

/// Загружает токен из `path` или создаёт новый.
///
/// Если файла нет: читаются 32 случайных байта из `/dev/urandom`,
/// hex-кодируются и записываются в файл с правами 0600
/// (родительский каталог создаётся с режимом по умолчанию 0755).
/// Если файл есть: содержимое читается, пробелы по краям игнорируются.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    match read_existing(path) {
        Ok(token) => Ok(token),
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_token(path),
        Err(e) => Err(e),
    }
}

/// Читает существующий токен; пустой файл — ошибка данных.
fn read_existing(path: &Path) -> io::Result<String> {
    let raw = fs::read_to_string(path)?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("файл токена {} пуст", path.display()),
        ));
    }
    Ok(token.to_string())
}

/// Генерирует токен и атомарно создаёт файл.
fn create_token(path: &Path) -> io::Result<String> {
    let token = hex(&random_bytes(TOKEN_BYTES)?);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match write_exclusively(path, token.as_bytes()) {
        Ok(()) => Ok(token),
        // Гонка: файл успел создать другой процесс — берём его содержимое.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_existing(path),
        Err(e) => Err(e),
    }
}

/// Читает `n` случайных байтов из системного источника энтропии.
fn random_bytes(n: usize) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(RANDOM_SOURCE)?;
    let mut buf = vec![0u8; n];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Hex-кодирует байты (нижний регистр).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Создаёт файл исключительно (`create_new`) и записывает содержимое.
#[cfg(unix)]
fn write_exclusively(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    // `mode(0o600)` маскируется umask — гарантируем точные права.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Создаёт файл исключительно (`create_new`) и записывает содержимое.
#[cfg(not(unix))]
fn write_exclusively(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Временный каталог без внешних крейтов: уникальный суффикс
    /// из pid и счётчика; удаляется при разрушении.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("bottle-agent-tests-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).expect("создать временный каталог");
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn first_call_creates_hex_token_of_64_chars() {
        let dir = TempDir::new();
        let path = dir.path("token");

        let token = load_or_create_token(&path).expect("создать токен");

        assert!(path.is_file(), "файл токена должен существовать");
        assert!(token.len() >= 64, "токен короче 64 символов: {token}");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "токен не hex: {token}"
        );
    }

    #[test]
    fn second_call_returns_same_token() {
        let dir = TempDir::new();
        let path = dir.path("token");

        let first = load_or_create_token(&path).expect("первый вызов");
        let second = load_or_create_token(&path).expect("второй вызов");

        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn created_token_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let path = dir.path("token");
        load_or_create_token(&path).expect("создать токен");

        let mode = fs::metadata(&path)
            .expect("метаданные")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "права файла токена не 0600");
    }

    #[test]
    fn existing_token_is_read_trimmed() {
        let dir = TempDir::new();
        let path = dir.path("token");
        fs::write(&path, "  deadbeef\n").expect("записать токен");

        let token = load_or_create_token(&path).expect("прочитать токен");

        assert_eq!(token, "deadbeef");
    }

    #[test]
    fn empty_existing_token_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path("token");
        fs::write(&path, "  \n").expect("записать пустой токен");

        assert!(load_or_create_token(&path).is_err());
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = TempDir::new();
        let path = dir.path("nested/dir/token");

        let token = load_or_create_token(&path).expect("создать токен в подкаталоге");

        assert!(path.is_file());
        assert!(token.len() >= 64);
    }
}
