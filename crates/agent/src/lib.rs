//! Библиотечная часть `bottle-agent`.
//!
//! Выделена из бинарного target'а, чтобы интеграционные тесты
//! (`tests/netns_lifecycle.rs`) могли вызывать [`netns::netns_up`] и
//! [`netns::netns_down`] напрямую.

pub mod control;
pub mod error;
pub mod netns;
pub mod relay_run;
pub mod token;
pub mod wg;
