//! Поведенческий тест операций над реальной таблицей маршрутов
//! (только Windows, запуск под администратором): `add` точечного
//! маршрута через шлюз по умолчанию → `Get-NetRoute` видит его →
//! `del` → маршрут отсутствует.
//!
//! Запуск: `cargo test -p svc --test routes_win -- --ignored`
//! (джоба CI `windows-checks`: раннеры под администратором, реальный
//! ActiveStore; TEST-NET-3 из RFC 5737 — адрес гарантированно ничей).

#![cfg(windows)]

use std::net::IpAddr;
use std::process::Command;

use svc::routes::{route_ps_args, run_ps, RouteCmd, RouteOpError};

/// Точечный маршрут на тестовый адрес TEST-NET-3 (RFC 5737).
const TEST_DEST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 77));

/// Префикс тестового маршрута в таблице маршрутов.
const TEST_PREFIX: &str = "203.0.113.77/32";

/// Выполняет PowerShell-скрипт и возвращает stdout — вспомогательный
/// канал проверки, независимый от проверяемого [`run_ps`].
fn ps(script: &str) -> String {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell.exe доступен на Windows-раннере");
    assert!(
        output.status.success(),
        "вспомогательный PowerShell-скрипт завершился с ошибкой: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Шлюз и интерфейс маршрута по умолчанию хоста (0.0.0.0/0, маршрут
/// с наименьшей метрикой): тест добавляет точечный маршрут рядом
/// с магистральным, не выдумывая топологию.
fn default_route() -> (IpAddr, String) {
    let script =
        "(Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Sort-Object RouteMetric | Select-Object -First 1)";
    let gateway = ps(&format!("{script}.NextHop"));
    let alias = ps(&format!("{script}.InterfaceAlias"));
    let gateway = gateway.trim().to_string();
    let alias = alias.trim().to_string();
    assert!(
        !gateway.is_empty() && !alias.is_empty(),
        "на хосте нет маршрута по умолчанию (0.0.0.0/0) — тест не может выбрать шлюз"
    );
    (
        gateway
            .parse()
            .expect("NextHop маршрута по умолчанию — IPv4-адрес"),
        alias,
    )
}

/// Сценарий roundtrip без паник внутри: любая неудача — Err, чтобы
/// внешний вызов успел убрать тестовый маршрут до падения теста.
async fn roundtrip(gateway: IpAddr, iface_alias: &str) -> Result<(), String> {
    let add = route_ps_args(RouteCmd::Add, TEST_DEST, Some(gateway), Some(iface_alias));
    let del = route_ps_args(RouteCmd::Del, TEST_DEST, Some(gateway), Some(iface_alias));
    let list = route_ps_args(RouteCmd::List, TEST_DEST, None, None);

    run_ps(&add)
        .await
        .map_err(|e| format!("New-NetRoute должен проходить под администратором: {e}"))?;

    let listing = run_ps(&list)
        .await
        .map_err(|e| format!("Get-NetRoute не должен завершаться с ошибкой: {e}"))?;
    if !listing.contains(TEST_PREFIX) {
        return Err(format!("маршрут не виден в Get-NetRoute:\n{listing}"));
    }

    run_ps(&del)
        .await
        .map_err(|e| format!("Remove-NetRoute должен проходить под администратором: {e}"))?;

    // Отсутствие маршрута — не ошибка: List идёт с SilentlyContinue и
    // обязан вернуть код выхода 0 на пустой выборке.
    let listing = run_ps(&list)
        .await
        .map_err(|e| format!("Get-NetRoute на отсутствующем маршруте не ошибка: {e}"))?;
    if listing.contains(TEST_PREFIX) {
        return Err(format!("маршрут остался после Remove-NetRoute:\n{listing}"));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "меняет реальную таблицу маршрутов (ActiveStore); запускать под администратором"]
async fn add_del_roundtrip_through_default_gateway() {
    let (gateway, iface_alias) = default_route();
    let outcome = roundtrip(gateway, &iface_alias).await;
    // Очистка в любом исходе (включая неудачный сценарий): остаток
    // не должен переживать прогон.
    let _ = run_ps(&route_ps_args(
        RouteCmd::Del,
        TEST_DEST,
        Some(gateway),
        Some(&iface_alias),
    ))
    .await;
    outcome.expect("add → Get-NetRoute → del → Get-NetRoute roundtrip");
}

#[tokio::test]
#[ignore = "держит PowerShell 30 с до срабатывания таймаута; Windows"]
async fn run_ps_times_out_on_hung_command() {
    // run_ps принимает любые аргументы сценария: зависший Start-Sleep
    // должен быть убит по таймауту, а не блокировать вызывающего.
    let result = run_ps(&[
        "Start-Sleep".to_string(),
        "-Seconds".to_string(),
        "120".to_string(),
    ])
    .await;
    assert!(
        matches!(result, Err(RouteOpError::TimedOut)),
        "ожидался таймаут 30 с, получено: {result:?}"
    );
}
