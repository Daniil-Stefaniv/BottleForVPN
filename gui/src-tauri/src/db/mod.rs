//! Хранилище GUI «Погребок» (0.3, спека §4): SQLCipher-БД `pogrebok.db`.
//!
//! Все данные приложения — бутылки, тексты конфигов VPN, логины/пароли —
//! живут в одном зашифрованном файле (AES-256, штатный KDF SQLCipher из
//! мастер-пароля). На диске открытых секретов больше нет.
//!
//! Пароль передаётся passphras'ой в `PRAGMA key` с экранированием кавычек
//! (unicode/пробелы/эмодзи — законные пароли, Review Focus #1). Неверный
//! пароль отличается от повреждённого файла контрольным значением
//! `meta.kdf_check` и эвристикой формата файла.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// Имя файла хранилища.
pub const VAULT_FILE_NAME: &str = "pogrebok.db";

/// Контрольное значение `meta.kdf_check`: известная строка, читаемая
/// только при верном ключе (неверный пароль ≠ повреждённая БД).
const KDF_CHECK: &str = "pogrebok-vault-ok";

/// Версия схемы этой сборки (`meta.schema_version`).
const SCHEMA_VERSION: &str = "1";

/// Схема хранилища (спека §4).
const SCHEMA: &str = "
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE bottles (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN ('env','host')),
  vpn_json    TEXT NOT NULL,
  expose_json TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE TABLE configs (
  bottle_id   TEXT PRIMARY KEY REFERENCES bottles(id) ON DELETE CASCADE,
  config_text TEXT NOT NULL
);
CREATE TABLE credentials (
  bottle_id  TEXT PRIMARY KEY REFERENCES bottles(id) ON DELETE CASCADE,
  username   TEXT NOT NULL,
  password   TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
PRAGMA foreign_keys = ON;
";

/// Зашифрованное хранилище: открытое (разблокированное) соединение.
pub struct Vault {
    pub(crate) conn: Connection,
}

/// Ошибка разблокировки хранилища.
#[derive(Debug, PartialEq, Eq)]
pub enum UnlockError {
    /// Неверный мастер-пароль (файл — настоящее хранилище, ключ не подошёл).
    WrongPassword,
    /// Файл повреждён, не является хранилищем или отсутствует.
    Corrupt(String),
    /// Хранилище занято другим процессом.
    Busy,
}

impl Vault {
    /// Существует ли файл хранилища по пути `path`.
    pub fn exists(path: &Path) -> bool {
        path.is_file()
    }

    /// Создаёт новое хранилище: схема, `meta` (schema_version, kdf_check,
    /// created_at), PRAGMA journal_mode=WAL, synchronous=NORMAL.
    /// Существующий файл — ошибка (не затираем чужие данные молча).
    pub fn create(path: &Path, password: &str) -> Result<Vault, String> {
        if path.exists() {
            return Err(format!(
                "хранилище уже существует: {} — удалите или переименуйте файл вручную",
                path.display()
            ));
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| format!("не удалось создать каталог {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| format!("не удалось создать хранилище: {e}"))?;
        apply_key(&conn, password).map_err(|e| format!("не удалось задать ключ: {e}"))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("не удалось создать схему хранилища: {e}"))?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('kdf_check', ?2)",
            rusqlite::params![SCHEMA_VERSION, KDF_CHECK],
        )
        .map_err(|e| format!("не удалось записать мету хранилища: {e}"))?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('created_at', ?1)",
            [now_utc()],
        )
        .map_err(|e| format!("не удалось записать мету хранилища: {e}"))?;
        apply_pragmas(&conn).map_err(|e| format!("не удалось настроить хранилище: {e}"))?;
        Ok(Vault { conn })
    }

    /// Разблокирует существующее хранилище паролем: контрольное чтение
    /// `meta.kdf_check` сразу после `PRAGMA key`.
    pub fn unlock(path: &Path, password: &str) -> Result<Vault, UnlockError> {
        if !path.is_file() {
            return Err(UnlockError::Corrupt(format!(
                "файл хранилища не найден: {}",
                path.display()
            )));
        }
        let conn = Connection::open(path)
            .map_err(|e| UnlockError::Corrupt(format!("не удалось открыть файл: {e}")))?;
        apply_key(&conn, password)
            .map_err(|e| UnlockError::Corrupt(format!("не удалось применить ключ: {e}")))?;
        // Контрольное значение: ошибка чтения/несовпадение — ключ не
        // подошёл, если файл вообще похож на хранилище SQLCipher.
        let check: Result<String, rusqlite::Error> = conn.query_row(
            "SELECT value FROM meta WHERE key = 'kdf_check'",
            [],
            |row| row.get(0),
        );
        match check {
            Ok(value) if value == KDF_CHECK => {}
            Ok(_) => {
                return Err(UnlockError::Corrupt(
                    "контрольное значение хранилища не совпадает — файл повреждён".to_string(),
                ))
            }
            Err(error) => return Err(classify_open_error(path, &error)),
        }
        apply_pragmas(&conn)
            .map_err(|e| UnlockError::Corrupt(format!("не удалось настроить хранилище: {e}")))?;
        Ok(Vault { conn })
    }

    /// Смена мастер-пароля (`PRAGMA rekey`; спека §3, настройки).
    pub fn rekey(&self, new_password: &str) -> Result<(), String> {
        let sql = format!("PRAGMA rekey = '{}'", escape_password(new_password));
        self.conn
            .execute_batch(&sql)
            .map_err(|e| format!("не удалось сменить мастер-пароль: {e}"))
    }
}

/// `PRAGMA key` с экранированной passphras'ой (одинарные кавычки
/// удваиваются — пароль не ломает SQL при любом unicode-содержимом).
fn apply_key(conn: &Connection, password: &str) -> rusqlite::Result<()> {
    let sql = format!("PRAGMA key = '{}'", escape_password(password));
    conn.execute_batch(&sql)
}

/// Экранирование passphras'ы для SQL-строкового литерала.
fn escape_password(password: &str) -> String {
    password.replace('\'', "''")
}

/// Рабочие PRAGMA хранилища (спека §4).
fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
}

/// Различает «неверный пароль» и «повреждённый/чужой файл» по ошибке
/// контрольного чтения и виду файла.
///
/// SQLCipher шифрует файл целиком — магии в заголовке нет, поэтому
/// криптографически «неверный ключ» и «мусорный файл» неотличимы.
/// Практическая граница (спека §10): файл с магией открытого SQLite
/// («SQLite format 3») или подозрительно маленький файл — точно не
/// хранилище (Corrupt); полноразмерный файл без магии — хранилище
/// с неверным паролем (WrongPassword).
fn classify_open_error(path: &Path, error: &rusqlite::Error) -> UnlockError {
    use rusqlite::ErrorCode;
    if let rusqlite::Error::SqliteFailure(e, _) = error {
        if e.code == ErrorCode::DatabaseBusy {
            return UnlockError::Busy;
        }
    }
    let meta = fs::metadata(path).ok();
    let tiny = meta.as_ref().is_some_and(|m| m.len() < 4096);
    let plain_magic = fs::File::open(path)
        .and_then(|mut f| {
            use io::Read;
            let mut head = [0u8; 16];
            f.read_exact(&mut head).map(|_| head)
        })
        .is_ok_and(|head| &head == b"SQLite format 3\0");
    if tiny || plain_magic {
        UnlockError::Corrupt(format!(
            "файл не похож на хранилище Погребка: {}",
            path.display()
        ))
    } else {
        UnlockError::WrongPassword
    }
}

/// Текущая метка UTC `YYYY-MM-DDTHH:MM:SSZ` без внешних зависимостей
/// (алгоритм Хиннанта — как в export.rs демона).
fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Неверный пароль — WrongPassword (не Corrupt); верный — открывает.
    #[test]
    fn wrong_password_is_not_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VAULT_FILE_NAME);
        Vault::create(&path, "s3cret").expect("создание");

        let wrong = Vault::unlock(&path, "other");
        assert!(
            matches!(wrong, Err(UnlockError::WrongPassword)),
            "неверный пароль отклонён как WrongPassword: {}",
            wrong.err().map(|e| format!("{e:?}")).unwrap_or_default()
        );

        Vault::unlock(&path, "s3cret").expect("верный пароль открывает");
    }

    /// Review Focus #1: пароли с пробелами, unicode и эмодзи живут
    /// полным циклом create → unlock → rekey → unlock.
    #[test]
    fn unicode_emoji_space_passwords_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VAULT_FILE_NAME);
        let first = "пароль с пробелами 🍾✨";
        let second = "кавычка ' и \t таб";
        let vault = Vault::create(&path, first).expect("создание");
        vault.rekey(second).expect("rekey");
        drop(vault);

        assert!(
            matches!(
                Vault::unlock(&path, first),
                Err(UnlockError::WrongPassword)
            ),
            "старый пароль не подходит"
        );
        Vault::unlock(&path, second).expect("новый пароль открывает");
    }

    /// Мусорный файл — Corrupt, не WrongPassword (и наоборот: настоящее
    /// хранилище с чужим паролем — именно WrongPassword).
    #[test]
    fn garbage_file_is_corrupt_not_wrong_password() {
        let tmp = tempfile::tempdir().unwrap();
        let garbage = tmp.path().join("garbage.db");
        fs::write(&garbage, "это вообще не база данных".as_bytes()).unwrap();
        assert!(matches!(
            Vault::unlock(&garbage, "любой"),
            Err(UnlockError::Corrupt(_))
        ));

        // Открытый (нешифрованный) SQLite-файл — тоже Corrupt.
        let plain = tmp.path().join("plain.db");
        Connection::open(&plain)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert!(matches!(
            Vault::unlock(&plain, "любой"),
            Err(UnlockError::Corrupt(_))
        ));
    }

    /// Схема и мета на месте после create; хранилище зашифровано
    /// (открытым SQLite не читается).
    #[test]
    fn create_writes_schema_meta_and_encrypts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VAULT_FILE_NAME);
        let vault = Vault::create(&path, "pw").expect("создание");

        let version: String = vault
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Файл не начинается с магии открытого SQLite — зашифрован.
        let head = fs::read(&path).unwrap();
        assert!(!head.starts_with(b"SQLite format 3\0"), "файл зашифрован");
        assert!(head.len() >= 4096, "хранилище полноразмерное");

        // Открытый движок файл не читает.
        let plain = Connection::open(&path).unwrap();
        assert!(plain.execute_batch("SELECT count(*) FROM meta").is_err());
    }

    /// exists: false до создания, true после.
    #[test]
    fn exists_tracks_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VAULT_FILE_NAME);
        assert!(!Vault::exists(&path));
        Vault::create(&path, "pw").unwrap();
        assert!(Vault::exists(&path));
    }
}
