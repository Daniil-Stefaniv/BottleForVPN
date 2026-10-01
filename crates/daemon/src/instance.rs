//! Single-instance демона: повторный запуск `bottled` невозможен.

/// Пытается стать единственным запущенным экземпляром демона.
///
/// Windows — именованный мьютекс `Global\bottled-singleton`: создаётся
/// и удерживается до конца процесса. `false` означает, что демона уже
/// запустили (мьютекс существует) либо мьютекс не создаётся. Прочие
/// платформы — всегда `true`: named-pipe транспорт существует только
/// на Windows, поэтому мьютекс нужен лишь там, а dev-запуски на
/// macOS/linux блокировать незачем.
pub fn acquire_single_instance() -> bool {
    #[cfg(windows)]
    {
        single_instance_mutex::acquire()
    }
    #[cfg(not(windows))]
    {
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
