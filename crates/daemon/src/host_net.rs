//! Host-сеть демона (M2 T8): резолв endpoint'ов host-бутылок и поиск
//! физического шлюза ОС.
//!
//! Endpoint'ы резолвятся системным резолвером (`(host, 0)
//! .to_socket_addrs()` — блокирующий вызов, async-обёртка
//! [`resolve_endpoints_async`] уходит в `spawn_blocking`); результат
//! дедуплицируется с сохранением порядка ([`collect_addrs`] — чистая,
//! unit-тестируемая).
//!
//! Физический шлюз — первый маршрут `0.0.0.0/0` по возрастанию
//! метрики интерфейса, чей адаптер не в списке исключений
//! (WSL/loopback/isatap/teredo и адаптеры host-бутылок по индексу).
//! Запрос идёт читающим PowerShell ([`physical_gateway_ps`]) — правила
//! квотирования значений см. [`crate::ps_query`]; разбор вывода —
//! чистый [`parse_gateway_table`] на фикс-фрагментах. Поведенческая
//! проверка (реальный `Get-NetRoute`) — windows-CI/ручная.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use crate::error::DaemonError;

/// Псевдонимы интерфейсов, исключаемые из поиска физического шлюза
/// по умолчанию (передаются в [`parse_gateway_table`] из
/// [`physical_gateway`]): виртуальный WSL-коммутатор, loopback и
/// туннельные псевдоинтерфейсы. Сравнение кейс-инсенситивно.
const DEFAULT_EXCLUDE_ALIAS_PARTS: &[&str] = &["vEthernet (WSL", "Loopback", "isatap", "teredo"];

/// Собирает уникальные IP-адреса из сокет-адресов резолвера.
///
/// Чистая часть резолва (std-резолвер не мокается): дедуп с
/// сохранением порядка первого вхождения, v4 и v6 смешиваются
/// как есть (маршруты `/32`//`128` добавляются на каждый).
pub fn collect_addrs(iter: impl Iterator<Item = SocketAddr>) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for addr in iter {
        let ip = addr.ip();
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Резолвит endpoint-хосты в IP-адреса (блокирующий вызов).
///
/// Хозяева передают системному резолверу `(host, 0)`; ошибки
/// отдельных хостов логируются `warn` (не fatal): неразрешимый
/// endpoint просто не получает pin-маршрута и идёт через туннель.
/// Пустой итоговый список (ничего не резолвится) — ошибка
/// «endpoint не резолвится» со списком хостов; из async-кода —
/// [`resolve_endpoints_async`].
pub fn resolve_endpoints(hosts: &[String]) -> Result<Vec<IpAddr>, DaemonError> {
    let mut socket_addrs = Vec::new();
    for host in hosts {
        match (host.as_str(), 0).to_socket_addrs() {
            Ok(resolved) => socket_addrs.extend(resolved),
            Err(error) => {
                tracing::warn!(host, %error, "endpoint-хост не резолвится");
            }
        }
    }
    let addrs = collect_addrs(socket_addrs.into_iter());
    if addrs.is_empty() {
        Err(DaemonError::HostNet(format!(
            "endpoint не резолвится: {}",
            hosts.join(", ")
        )))
    } else {
        Ok(addrs)
    }
}

/// Async-обёртка [`resolve_endpoints`]: блокирующий резолв уходит в
/// `spawn_blocking`, чтобы не занимать runtime демона.
pub async fn resolve_endpoints_async(hosts: &[String]) -> Result<Vec<IpAddr>, DaemonError> {
    let hosts = hosts.to_vec();
    tokio::task::spawn_blocking(move || resolve_endpoints(&hosts))
        .await
        .map_err(|e| DaemonError::HostNet(format!("задача резолва endpoint'ов отменена: {e}")))?
}

/// Собирает аргументы PowerShell-запроса физических маршрутов:
/// `Get-NetRoute -DestinationPrefix '0.0.0.0/0'` (пустая выборка — не
/// ошибка: `SilentlyContinue`, как в svc/routes.rs), сортировка по
/// метрике интерфейса, текстовые колонки без заголовков. `-Width 512`
/// страховает от усечения длинных имён адаптеров `Format-Table`
/// (усечённый «vEthernet (WS…» перестал бы матчиться фильтром).
pub fn physical_gateway_ps() -> Vec<String> {
    vec![
        "Get-NetRoute".to_string(),
        "-DestinationPrefix".to_string(),
        crate::ps_query::ps_value("0.0.0.0/0"),
        // Пустая выборка (нет default route) — не ошибка, пустой вывод
        // (прецедент: List в svc/routes.rs).
        "-ErrorAction".to_string(),
        "SilentlyContinue".to_string(),
        "|".to_string(),
        "Sort-Object".to_string(),
        "-Property".to_string(),
        "InterfaceMetric".to_string(),
        "|".to_string(),
        "Select-Object".to_string(),
        "NextHop,".to_string(),
        "InterfaceAlias,".to_string(),
        "InterfaceIndex,".to_string(),
        "InterfaceMetric".to_string(),
        "|".to_string(),
        "Format-Table".to_string(),
        "-HideTableHeaders".to_string(),
        "|".to_string(),
        "Out-String".to_string(),
        // Без явной ширины Format-Table режет колонки по консольным
        // 120 символам: усечённый алиас «vEthernet (WS…» перестал бы
        // матчиться фильтром — выбираем физику по ошибке.
        "-Width".to_string(),
        "512".to_string(),
    ]
}

/// Разбирает вывод `physical_gateway_ps`: строки-колонки
/// `NextHop InterfaceAlias InterfaceIndex InterfaceMetric` (разделение
/// по whitespace; псевдоним с пробелами — всё между первым токеном и
/// двумя последними числами).
///
/// Фильтры: `InterfaceIndex` в `exclude_idx` (адаптеры host-бутылок),
/// псевдоним содержит любую из `exclude_alias_parts` (кейс-
/// инсенситивно; паттерны передаёт вызвавший — по умолчанию их нет).
/// On-link маршруты (`NextHop 0.0.0.0`) пропускаются: unspecified —
/// не адрес шлюза, pin-маршрут через него молча разъюздился бы.
/// Первый прошедший ряд — ответ; пусто — «физический шлюз не найден».
pub fn parse_gateway_table(
    text: &str,
    exclude_idx: &[u32],
    exclude_alias_parts: &[&str],
) -> Result<(IpAddr, String), DaemonError> {
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        // Минимум: NextHop, псевдоним, индекс, метрика; мусор
        // (пустые строки, обрывки переноса) пропускается.
        if tokens.len() < 4 {
            continue;
        }
        let Ok(next_hop) = tokens[0].parse::<IpAddr>() else {
            continue;
        };
        // On-link default (NextHop 0.0.0.0) — не адрес шлюза.
        if next_hop.is_unspecified() {
            continue;
        }
        // Два последних токена — InterfaceIndex и InterfaceMetric;
        // не-число означает мусорную строку.
        let Ok(iface_idx) = tokens[tokens.len() - 2].parse::<u32>() else {
            continue;
        };
        if tokens[tokens.len() - 1].parse::<u32>().is_err() {
            continue;
        }
        // Псевдоним с пробелами — всё между NextHop и парой чисел.
        let alias = tokens[1..tokens.len() - 2].join(" ");
        if exclude_idx.contains(&iface_idx) {
            continue;
        }
        let alias_lower = alias.to_lowercase();
        if exclude_alias_parts
            .iter()
            .any(|part| alias_lower.contains(&part.to_lowercase()))
        {
            continue;
        }
        return Ok((next_hop, alias));
    }
    Err(DaemonError::HostNet(
        "физический шлюз не найден".to_string(),
    ))
}

/// Физический шлюз ОС: запуск [`physical_gateway_ps`] (только
/// Windows) и разбор вывода с фильтрами по умолчанию
/// ([`DEFAULT_EXCLUDE_ALIAS_PARTS`]) и индексам адаптеров
/// host-бутылок вызывающего.
pub async fn physical_gateway(exclude_idx: &[u32]) -> Result<(IpAddr, String), DaemonError> {
    #[cfg(not(windows))]
    {
        let _ = exclude_idx;
        Err(DaemonError::HostNet(
            "поиск физического шлюза доступен только на Windows".to_string(),
        ))
    }
    #[cfg(windows)]
    {
        let output = crate::ps_query::run_ps_readonly(&physical_gateway_ps()).await?;
        parse_gateway_table(&output, exclude_idx, DEFAULT_EXCLUDE_ALIAS_PARTS)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, SocketAddrV4, SocketAddrV6};

    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    fn sock4(text: &str) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(text.parse().expect("тестовый IPv4"), 0))
    }

    fn sock6(text: &str) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(
            text.parse().expect("тестовый IPv6"),
            0,
            0,
            0,
        ))
    }

    #[test]
    fn collect_addrs_dedups_preserving_order_across_families() {
        let addrs = collect_addrs(
            [
                sock4("8.8.8.8"),
                sock6("2606:4700::1111"),
                sock4("8.8.8.8"),
                sock4("1.1.1.1"),
                sock6("2606:4700::1111"),
            ]
            .into_iter(),
        );
        assert_eq!(
            addrs,
            vec![ip("8.8.8.8"), ip("2606:4700::1111"), ip("1.1.1.1")],
            "дедуп по первому вхождению, v4/v6 смешаны"
        );
    }

    #[test]
    fn collect_addrs_empty_iterator_gives_empty_vec() {
        assert!(collect_addrs(std::iter::empty()).is_empty());
    }

    #[test]
    fn resolve_endpoints_empty_hosts_is_error() {
        let error = resolve_endpoints(&[]).expect_err("нечего резолвить");
        assert!(
            matches!(error, DaemonError::HostNet(ref message) if message.contains("не резолвится")),
            "{error}"
        );
    }

    /// Литеральные хосты резолвятся std без DNS — пин тонкой обёртки:
    /// дедуп между хостами и смешение семейств.
    #[test]
    fn resolve_endpoints_literal_hosts_dedup_across_hosts() {
        let hosts = ["198.51.100.5".to_string(), "198.51.100.5".to_string()];
        assert_eq!(
            resolve_endpoints(&hosts).expect("литералы резолвятся"),
            vec![ip("198.51.100.5")],
            "повтор хоста даёт один адрес"
        );
        let hosts = ["198.51.100.5".to_string(), "2001:db8::1".to_string()];
        assert_eq!(
            resolve_endpoints(&hosts).expect("литералы резолвятся"),
            vec![ip("198.51.100.5"), ip("2001:db8::1")]
        );
    }

    #[test]
    fn physical_gateway_ps_pins_readonly_query_shape() {
        let args = physical_gateway_ps();
        let expected: Vec<&str> = vec![
            "Get-NetRoute",
            "-DestinationPrefix",
            // Значение-префикс — PS-литерал в одинарных кавычках
            // (дисциплина ps_value), хотя префикс и константа.
            "'0.0.0.0/0'",
            // Пустая выборка — не ошибка (как List в svc/routes.rs).
            "-ErrorAction",
            "SilentlyContinue",
            "|",
            "Sort-Object",
            "-Property",
            "InterfaceMetric",
            "|",
            "Select-Object",
            "NextHop,",
            "InterfaceAlias,",
            "InterfaceIndex,",
            "InterfaceMetric",
            "|",
            "Format-Table",
            "-HideTableHeaders",
            "|",
            "Out-String",
            // Без ширины Format-Table усекает длинные имена адаптеров.
            "-Width",
            "512",
        ];
        assert_eq!(args, expected, "точная форма запроса");
        // Запрос только читает: нет изменяющих командлетов.
        assert!(!args.iter().any(|a| a.contains("New-NetRoute")));
        assert!(!args.iter().any(|a| a.contains("Remove-NetRoute")));
    }

    #[test]
    fn parse_gateway_table_normal_table_picks_first_row_with_spaced_alias() {
        // PS уже отсортировал по метрике; парсер берёт первый ряд.
        // «Ethernet 2» — псевдоним с пробелом (токены: gw alias idx metric).
        let table = "10.0.0.1  Ethernet 2  11  10\n192.168.1.1  Wi-Fi  12  25\n";
        let (gateway, alias) = parse_gateway_table(table, &[], &[]).expect("первый ряд подходит");
        assert_eq!(gateway, ip("10.0.0.1"));
        assert_eq!(alias, "Ethernet 2");
    }

    #[test]
    fn parse_gateway_table_filters_wsl_aliases_case_insensitively() {
        let table = "172.20.16.1  vEthernet (WSLDefault Switch)  28  5000\n\
                     192.168.1.1  Wi-Fi  12  25\n";
        let (gateway, alias) = parse_gateway_table(
            table,
            &[],
            &["vEthernet (WSL", "Loopback", "isatap", "teredo"],
        )
        .expect("WSL отфильтрован, физика выбрана");
        assert_eq!(gateway, ip("192.168.1.1"));
        assert_eq!(alias, "Wi-Fi");

        // Кейс-инсенситивность: uppercase-вариант паттерна тоже матчится.
        let upper =
            "172.20.16.1  VETHERNET (WSLDEFAULT SWITCH)  28  5000\n192.168.1.1  Wi-Fi  12  25\n";
        let (gateway, _) = parse_gateway_table(upper, &[], &["vEthernet (WSL"]).expect("фильтр");
        assert_eq!(gateway, ip("192.168.1.1"));
    }

    #[test]
    fn parse_gateway_table_exclude_idx_drops_row() {
        let table = "192.168.1.1  Wi-Fi  12  25\n10.0.0.1  Ethernet  11  30\n";
        let (gateway, alias) =
            parse_gateway_table(table, &[12], &[]).expect("первый ряд исключён по индексу");
        assert_eq!(gateway, ip("10.0.0.1"));
        assert_eq!(alias, "Ethernet");
    }

    #[test]
    fn parse_gateway_table_empty_or_all_filtered_is_error() {
        let error = parse_gateway_table("\n \n", &[], &[]).expect_err("пустая таблица");
        assert!(
            matches!(error, DaemonError::HostNet(ref m) if m.contains("физический шлюз не найден")),
            "{error}"
        );
        let wsl_only = "172.20.16.1  vEthernet (WSL)  28  5000\n";
        assert!(
            parse_gateway_table(wsl_only, &[], &["vEthernet (WSL"]).is_err(),
            "все ряды отфильтрованы — ошибка"
        );
    }

    /// Мусорные ряды (обрывки, не-IP, on-link 0.0.0.0) пропускаются,
    /// а не валят разбор.
    #[test]
    fn parse_gateway_table_skips_garbage_and_onlink_rows() {
        let table = "\n\
                     продолжение-оборванной-строки\n\
                     0.0.0.0  Wi-Fi  12  25\n\
                     not-an-ip  Ethernet  11  30\n\
                     192.168.1.1  Wi-Fi  12  25\n";
        let (gateway, alias) =
            parse_gateway_table(table, &[], &[]).expect("мусор пропущен, годный ряд выбран");
        assert_eq!(gateway, ip("192.168.1.1"), "on-link 0.0.0.0 — не шлюз");
        assert_eq!(alias, "Wi-Fi");
    }
}
