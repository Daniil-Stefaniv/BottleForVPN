//! Single-instance демона: повторный запуск `bottled` невозможен.

use std::path::Path;

/// Пытается стать единственным запущенным экземпляром демона.
///
/// Windows — именованный мьютекс `Global\bottled-singleton`: создаётся
/// и удерживается до конца процесса. Unix (M4: UDS-транспорт) —
/// эксклюзивный flock на `<data>/bottled.lock`: он закрывает гонку
/// одновременного старта двух демонов до того, как кто-то из них
/// займёт сокет (живой сокет дополнительно защищает [`crate::uds::serve`]).
/// `false` означает, что демона уже запустили либо лок не ставится.
pub fn acquire_single_instance(data_dir: &Path) -> bool {
    #[cfg(windows)]
    {
        let _ = data_dir; // мьютекс глобальный, каталог не нужен
        single_instance_mutex::acquire()
    }
    #[cfg(unix)]
    {
        single_instance_flock::acquire(data_dir)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = data_dir;
        true
    }
}

/// Реализация single-instance на `CreateMutexW`.
#[cfg(windows)]
mod single_instance_mutex {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    /// Имя мьютекса single-instance (глобальное пространство имён объектов).
    const MUTEX_NAME: &str = r"Global\bottled-singleton";

    /// Создаёт мьютекс и удерживает его дескриптор до конца процесса:
    /// пока демон жив, повторный `acquire` видит `ERROR_ALREADY_EXISTS`.
    pub fn acquire() -> bool {
        let name: Vec<u16> = MUTEX_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: имя — валидная NUL-терминированная UTF-16 строка,
        // атрибуты безопасности не передаются (NULL). Дескриптор
        // сознательно не закрывается: мьютекс должен жить до конца
        // процесса, ядро освободит его при выходе.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            tracing::warn!("не удалось создать мьютекс single-instance");
            return false;
        }
        // ERROR_ALREADY_EXISTS: мьютекс создан раньше нас другим процессом.
        let last_error = unsafe { GetLastError() };
        last_error != ERROR_ALREADY_EXISTS
    }
}

/// Реализация single-instance на flock (unix): lock-файл в каталоге
/// данных удерживается до конца процесса.
#[cfg(unix)]
mod single_instance_flock {
    use std::fs::File;
    use std::path::Path;
    use std::sync::OnceLock;

    /// Имя lock-файла внутри каталога данных демона.
    const LOCK_FILE: &str = "bottled.lock";

    /// Удерживаемый lock (живёт до конца процесса).
    static HELD: OnceLock<File> = OnceLock::new();

    /// Ставит эксклюзивный неблокирующий flock на `<data>/bottled.lock`.
    /// Файл (0600) остаётся на диске после выхода — это нормально:
    /// flock принадлежит открытому дескриптору, не файлу.
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    pub fn acquire(data_dir: &Path) -> bool {
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(data_dir.join(LOCK_FILE))
        else {
            tracing::warn!("не удалось открыть lock-файл single-instance");
            return false;
        };
        // SAFETY: flock(2) на валидном fd.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return false;
        }
        HELD.set(file).is_ok()
    }
}
