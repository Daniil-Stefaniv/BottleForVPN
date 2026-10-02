//! Библиотечная часть `bottle-svc`.
//!
//! Выделена из бинарного target'а, чтобы поведенческие тесты
//! (`tests/routes_win.rs`) могли вызывать [`routes::route_ps_args`]
//! и [`routes::run_ps`] напрямую без запуска службы.

pub mod routes;
