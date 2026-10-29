//! Библиотечная часть `pogrebok-svc`.
//!
//! Выделена из бинарного target'а, чтобы поведенческие тесты
//! (`tests/routes_win.rs`, `tests/svc_win.rs`) могли вызывать
//! [`routes::route_ps_args`], [`routes::run_ps`] и журнальное ядро
//! напрямую без запуска службы.

pub mod journal;
pub mod logging;
pub mod pipe;
pub mod routes;
