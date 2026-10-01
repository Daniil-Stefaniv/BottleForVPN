//! Ошибки демона BottleForApps.
//!
//! Строки ошибок хранилища профилей попадают пользователю (CLI → IPC),
//! поэтому они на русском языке — как в `agent::error`.

use std::io;
use std::path::PathBuf;

use bottle_core::profile::ProfileError;

/// Ошибка операции хранилища профилей.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Ошибка ввода-вывода (чтение/запись файлов хранилища).
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] io::Error),
    /// Профиль не прошёл разбор или валидацию (`bottle_core::profile`).
    #[error("некорректный профиль: {0}")]
    Profile(#[from] ProfileError),
    /// Файл `bottles/<id>.toml` не найден.
    #[error("профиль не найден: {id}")]
    NotFound {
        /// Идентификатор запрошенной бутылки.
        id: String,
    },
    /// Поле `id` внутри файла не совпадает с именем файла.
    #[error("идентификатор в файле ({profile_id}) не совпадает с именем файла ({file_id})")]
    IdMismatch {
        /// Идентификатор, взятый из имени файла.
        file_id: String,
        /// Значение поля `id` внутри профиля.
        profile_id: String,
    },
    /// Исходный файл конфигурации VPN, который `Store::save` копирует
    /// в хранилище, не найден.
    #[error("исходный файл конфигурации VPN не найден: {path}")]
    SourceConfigMissing {
        /// Путь к отсутствующему исходному файлу.
        path: PathBuf,
    },
}
