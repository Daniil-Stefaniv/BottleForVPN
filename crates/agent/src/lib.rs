//! Библиотечная часть `bottle-agent`.
//!
//! Выделена из бинарного target'а, чтобы интеграционные тесты
//! (`tests/netns_lifecycle.rs`) могли вызывать [`netns::netns_up`] и
//! [`netns::netns_down`] напрямую.

pub mod bottles;
pub mod control;
pub mod error;
pub mod netns;
pub mod relay_run;
pub mod token;
pub mod wg;

/// Утилиты только для юнит-тестов крейта.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    /// Сериализует тесты, работающие с эфемерными портами ОС.
    ///
    /// Тесты netns (bind_bridges) и relay_run (run_relay) каждый берут
    /// свободный порт через bind-пробы и тут же перевязывают его: при
    /// параллельном запуске один тест успевает захватить чужой пробный
    /// порт долгоживущим слушателем, и наблюдение «порт освободился»
    /// ломается ошибкой AddrInUse (проявляется на macOS). Блокировка на
    /// время теста делает каждую пробу детерминированной.
    pub(crate) static EPHEMERAL_PORT_LOCK: Mutex<()> = Mutex::new(());
}
