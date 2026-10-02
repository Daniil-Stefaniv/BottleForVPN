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

use svc::routes::{route_ps_args, run_ps, RouteCmd};

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

/// Убирает тестовый маршрут при любом исходе теста (включая панику
/// между add и del): остаток не должен переживать прогон.
struct RouteCleanup {
    /// Назначение маршрута.
    dest: IpAddr,
    /// Шлюз маршрута по умолчанию.
    gateway: IpAddr,
    /// Интерфейс шлюза.
    iface_alias: String,
}

impl Drop for RouteCleanup {
    fn drop(&mut self) {
        let _ = run_ps(&route_ps_args(
            RouteCmd::Del,
            self.dest,
            Some(self.gateway),
            Some(&self.iface_alias),
        ));
    }
}

#[test]
#[ignore = "меняет реальную таблицу маршрутов (ActiveStore); запускать под администратором"]
fn add_del_roundtrip_through_default_gateway() {
    let (gateway, iface_alias) = default_route();
    let _cleanup = RouteCleanup {
        dest: TEST_DEST,
        gateway,
        iface_alias: iface_alias.clone(),
    };

    // Возможные остатки прошлых прогонов не должны ломать тест.
    let _ = run_ps(&route_ps_args(
        RouteCmd::Del,
        TEST_DEST,
        Some(gateway),
        Some(&iface_alias),
    ));

    // Add: точечный маршрут через шлюз по умолчанию на его интерфейсе.
    run_ps(&route_ps_args(
        RouteCmd::Add,
        TEST_DEST,
        Some(gateway),
        Some(&iface_alias),
    ))
    .expect("New-NetRoute должен проходить под администратором");

    // Get-NetRoute (проверяемым конструктором List) видит маршрут.
    let listing = run_ps(&route_ps_args(RouteCmd::List, TEST_DEST, None, None))
        .expect("Get-NetRoute не должен завершаться с ошибкой");
    assert!(
        listing.contains(TEST_PREFIX),
        "маршрут не виден в Get-NetRoute:\n{listing}"
    );

    // Del: маршрут удаляется и больше не наблюдается.
    run_ps(&route_ps_args(
        RouteCmd::Del,
        TEST_DEST,
        Some(gateway),
        Some(&iface_alias),
    ))
    .expect("Remove-NetRoute должен проходить под администратором");
    let listing = run_ps(&route_ps_args(RouteCmd::List, TEST_DEST, None, None))
        .expect("Get-NetRoute не должен завершаться с ошибкой");
    assert!(
        !listing.contains(TEST_PREFIX),
        "маршрут остался после Remove-NetRoute:\n{listing}"
    );
}
