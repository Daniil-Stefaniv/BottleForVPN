//! Операции над таблицей маршрутов ОС (Windows) через командлеты
//! NetTCPIP: `New-NetRoute`/`Remove-NetRoute`/`Get-NetRoute` с
//! `-PolicyStore ActiveStore` — маршруты живут до перезагрузки,
//! живучесть обеспечивается сверкой при старте службы (задача 5).
//!
//! Единая точка построения точечных префиксов — [`route_ps_args`]:
//! `/32` для IPv4, `/128` для IPv6 (чистая функция, unit-тестируется
//! на любой ОС); фактический запуск PowerShell — [`run_ps`] через
//! `-EncodedCommand` (base64 от UTF-16LE) — один однозначный путь
//! разбора, без матрицы экранирования токенов.

use std::net::IpAddr;
use std::time::Duration;

/// Предельное время выполнения PowerShell-командлета: зависший
/// процесс не должен блокировать цикл службы навсегда.
// На не-windows используется только windows-реализацией run_ps.
#[cfg_attr(not(windows), allow(dead_code))]
const PS_TIMEOUT: Duration = Duration::from_secs(30);

/// Команда над таблицей маршрутов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteCmd {
    /// Добавить точечный маршрут (`New-NetRoute`).
    Add,
    /// Удалить точечный маршрут (`Remove-NetRoute`).
    Del,
    /// Запросить маршруты по префиксу назначения (`Get-NetRoute`).
    List,
}

impl RouteCmd {
    /// Командлет NetTCPIP для команды.
    fn cmdlet(self) -> &'static str {
        match self {
            RouteCmd::Add => "New-NetRoute",
            RouteCmd::Del => "Remove-NetRoute",
            RouteCmd::List => "Get-NetRoute",
        }
    }
}

/// Ошибка операции над маршрутами.
#[derive(Debug, thiserror::Error)]
pub enum RouteOpError {
    /// PowerShell завершился с ненулевым кодом; текст — stderr.
    #[error("PowerShell завершился с ошибкой: {stderr}")]
    PsFailed { stderr: String },
    /// Не удалось запустить PowerShell.
    #[error("не удалось запустить PowerShell: {0}")]
    Spawn(#[from] std::io::Error),
    /// PowerShell не завершился за [`PS_TIMEOUT`] — процесс убит.
    #[error("PowerShell не завершился вовремя (таймаут 30 с)")]
    TimedOut,
    /// Операции над маршрутами поддержаны только на Windows.
    #[error("операции над маршрутами доступны только на Windows")]
    NotWindows,
}

/// Точечный префикс назначения: `/32` для IPv4, `/128` для IPv6.
fn host_prefix(dest: IpAddr) -> String {
    match dest {
        IpAddr::V4(_) => format!("{dest}/32"),
        IpAddr::V6(_) => format!("{dest}/128"),
    }
}

/// Значение-аргумент командлета в PS-одинарных кавычках. Внутри них
/// `$`, `;`, backtick, `|`, скобки — литералы; единственное экранирование —
/// удвоение самой одинарной кавычки. Флаги и имена командлетов НЕ
/// квотируются: квотированный `-Confirm:$false` перестал бы быть
/// переключателем (PS принял бы его как позиционную строку).
fn ps_value(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// Собирает аргументы PowerShell-командлета для операции над маршрутом.
///
/// Общее начало: командлет + `-DestinationPrefix <ip>/32|/128`; при
/// заданном `gateway` добавляется пара `-NextHop <gw>`, при заданном
/// `iface_alias` — пара `-InterfaceAlias <alias>`. Значения-аргументы
/// оборачиваются в PS-одинарные кавычки ([`ps_value`]) — метасимволы
/// языка PowerShell внутри данных (имя интерфейса вида `Ethernet|cmd`)
/// интерпретироваться не могут; флаги и командлеты остаются голыми.
/// `Add`/`Del` пишут только в активное хранилище (`-PolicyStore
/// ActiveStore`) и падают при сбое (`-ErrorAction Stop`); `Del`
/// дополнительно отключает запрос подтверждения (`-Confirm:$false`).
/// `List` — читающий запрос по префиксу назначения с `-ErrorAction
/// SilentlyContinue`: отсутствующий маршрут — это пустой результат и
/// код выхода 0 (Get-NetRoute пишет non-terminating error на пустую
/// выборку), а не ошибка.
pub fn route_ps_args(
    cmd: RouteCmd,
    dest: IpAddr,
    gateway: Option<IpAddr>,
    iface_alias: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        cmd.cmdlet().to_string(),
        "-DestinationPrefix".to_string(),
        ps_value(&host_prefix(dest)),
    ];
    if let Some(gateway) = gateway {
        args.push("-NextHop".to_string());
        args.push(ps_value(&gateway.to_string()));
    }
    if let Some(alias) = iface_alias {
        args.push("-InterfaceAlias".to_string());
        args.push(ps_value(alias));
    }
    match cmd {
        RouteCmd::Add | RouteCmd::Del => {
            args.extend([
                "-PolicyStore".to_string(),
                "ActiveStore".to_string(),
                "-ErrorAction".to_string(),
                "Stop".to_string(),
            ]);
        }
        RouteCmd::List => {
            args.extend(["-ErrorAction".to_string(), "SilentlyContinue".to_string()]);
        }
    }
    if matches!(cmd, RouteCmd::Del) {
        args.push("-Confirm:$false".to_string());
    }
    args
}

/// Кодирует аргументы в значение параметра `-EncodedCommand`
/// PowerShell: аргументы, соединённые пробелами в строку скрипта →
/// UTF-16LE → base64 (документированный формат ключа). Единственный
/// путь разбора — сам PowerShell, без экранирования токенов.
pub fn encode_ps_command(args: &[String]) -> String {
    let script = args.join(" ");
    let mut utf16le = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16le.extend_from_slice(&unit.to_le_bytes());
    }
    base64_encode(&utf16le)
}

/// Кодирует байты в стандартный base64 (RFC 4648, базовый алфавит,
/// с паддингом). Нужен только для [`encode_ps_command`] — внешний
/// crate не заводился, корректность зафиксирована эталонами RFC 4648.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(triple[0]) << 16) | (u32::from(triple[1]) << 8) | u32::from(triple[2]);
        out.push(ALPHABET[(n >> 18) as usize & 0b111111] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0b111111] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0b111111] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0b111111] as char
        } else {
            '='
        });
    }
    out
}

/// Запускает PowerShell-командлет над таблицей маршрутов и возвращает
/// stdout. Аргументы строятся ТОЛЬКО здесь, через [`route_ps_args`]:
/// вызывающие не собирают PS-токены руками (рулинг M3-бэклога).
///
/// `powershell -NoProfile -NonInteractive -EncodedCommand <encode>` —
/// сценарий передаётся одним base64-токеном (UTF-16LE), поэтому
/// разбор не зависит от кавычек/метасимволов оболочки. Зависший
/// PowerShell снимается по таймауту [`PS_TIMEOUT`] (`kill_on_drop`).
/// Ненулевой код выхода → [`RouteOpError::PsFailed`] со stderr.
#[cfg(windows)]
pub async fn run_ps(
    cmd: RouteCmd,
    dest: IpAddr,
    gateway: Option<IpAddr>,
    iface_alias: Option<&str>,
) -> Result<String, RouteOpError> {
    run_ps_raw(&route_ps_args(cmd, dest, gateway, iface_alias)).await
}

/// Запуск произвольного PS-сценария: диагностический/тестовый путь
/// (таймаут-тест с зависшим `Start-Sleep`); продакшн-код таблицы
/// маршрутов проходит через [`run_ps`].
#[cfg(windows)]
pub async fn run_ps_raw(args: &[String]) -> Result<String, RouteOpError> {
    let child = tokio::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-EncodedCommand")
        .arg(encode_ps_command(args))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RouteOpError::Spawn)?;
    let output = match tokio::time::timeout(PS_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output.map_err(RouteOpError::Spawn)?,
        // Будущее wait_with_output снято — kill_on_drop убивает процесс.
        Err(_elapsed) => return Err(RouteOpError::TimedOut),
    };
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(RouteOpError::PsFailed {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Не-Windows стаб: операции над маршрутами недоступны.
#[cfg(not(windows))]
pub async fn run_ps(
    _cmd: RouteCmd,
    _dest: IpAddr,
    _gateway: Option<IpAddr>,
    _iface_alias: Option<&str>,
) -> Result<String, RouteOpError> {
    Err(RouteOpError::NotWindows)
}

/// Не-Windows стаб произвольного сценария (см. windows-версию).
#[cfg(not(windows))]
#[allow(dead_code)]
pub async fn run_ps_raw(_args: &[String]) -> Result<String, RouteOpError> {
    Err(RouteOpError::NotWindows)
}

// ----------------------------------------------------------------------
// Полная валидация шлюза (спека §4.2 дословно, M3).
// ----------------------------------------------------------------------

/// Строка таблицы default-маршрутов: псевдоним интерфейса и его
/// текущий next-hop (шлюз по умолчанию).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRouteRow {
    /// `InterfaceAlias` интерфейса default-маршрута.
    pub iface_alias: String,
    /// `NextHop` default-маршрута интерфейса.
    pub next_hop: IpAddr,
}

/// Разбирает вывод `Get-NetRoute -DestinationPrefix <default> |
/// Select-Object InterfaceAlias,NextHop | ConvertTo -Csv`: заголовок
/// «InterfaceAlias","NextHop», далее строки данных. Пустые NextHop
/// (on-link default-маршруты без шлюза) пропускаются: они не могут
/// быть шлюзом точечного маршрута.
pub fn parse_default_routes(csv: &str) -> Vec<DefaultRouteRow> {
    let mut rows = Vec::new();
    for line in csv.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Формат ConvertTo-Csv: "alias","nexthop" — простое
        // разрезание по «",\u{22}» (три символа: кавычка-запятая-
        // кавычка) достаточно для двух полей.
        let fields: Vec<&str> = line.split("\",\"").collect();
        if fields.len() != 2 {
            continue;
        }
        let alias = fields[0].trim_start_matches('"');
        let hop = fields[1].trim_end_matches('"');
        if let Ok(next_hop) = hop.parse::<IpAddr>() {
            rows.push(DefaultRouteRow {
                iface_alias: alias.to_string(),
                next_hop,
            });
        }
    }
    rows
}

/// Проверка «шлюз == текущий шлюз интерфейса» (спека §4.2): сверка
/// с таблицей default-маршрутов семейства запрошенного шлюза.
/// Ошибки — русские, единый текст «шлюз не соответствует интерфейсу»
/// для обоих случаев: шлюз чужой ИЛИ у интерфейса нет default-маршрута.
pub fn check_gateway_matches(
    gateway: IpAddr,
    iface_alias: &str,
    table: &[DefaultRouteRow],
) -> Result<(), String> {
    let matches_iface = table
        .iter()
        .find(|row| row.iface_alias == iface_alias && row.next_hop.is_ipv4() == gateway.is_ipv4())
        .is_some_and(|row| row.next_hop == gateway);
    if matches_iface {
        Ok(())
    } else {
        Err(format!(
            "шлюз {gateway} не соответствует интерфейсу {iface_alias}:              шлюз должен быть текущим шлюзом по умолчанию этого интерфейса"
        ))
    }
}

/// TTL кэша таблицы default-маршрутов (анти-шторм PowerShell при
/// старте нескольких бутылок подряд, план M3).
pub const GATEWAY_CACHE_TTL: Duration = Duration::from_secs(5);

/// Источник таблицы default-маршрутов; инъекция для тестов диспетчера.
#[async_trait::async_trait]
pub trait DefaultRouteSource: Send + Sync {
    /// Default-маршруты семейства: `v6` — `::/0`, иначе `0.0.0.0/0`.
    async fn default_routes(&self, v6: bool) -> Result<Vec<DefaultRouteRow>, RouteOpError>;
}

/// Продакшн-источник: `Get-NetRoute -DestinationPrefix <default> |
/// Select-Object InterfaceAlias,NextHop | ConvertTo-Csv` —
/// детерминированный CSV вместо парсинга широкой таблицы.
pub struct RealDefaultRoutes;

#[async_trait::async_trait]
impl DefaultRouteSource for RealDefaultRoutes {
    async fn default_routes(&self, v6: bool) -> Result<Vec<DefaultRouteRow>, RouteOpError> {
        #[cfg(windows)]
        {
            let prefix = if v6 { "::/0" } else { "0.0.0.0/0" };
            let args = vec![
                "Get-NetRoute".to_string(),
                "-DestinationPrefix".to_string(),
                ps_value(prefix),
                "|".to_string(),
                "Select-Object".to_string(),
                "InterfaceAlias,NextHop".to_string(),
                "|".to_string(),
                "ConvertTo-Csv".to_string(),
                "-NoTypeInformation".to_string(),
            ];
            let out = run_ps_raw(&args).await?;
            Ok(parse_default_routes(&out))
        }
        #[cfg(not(windows))]
        {
            let _ = v6;
            Err(RouteOpError::NotWindows)
        }
    }
}

/// Ошибка сверки шлюза: расхождение → VALIDATION-отказ; недоступность
/// таблицы → IO-отказ (сбой сети — не ошибка валидации).
#[derive(Debug)]
pub enum GatewayVerifyError {
    /// Шлюз не соответствует интерфейсу (или у интерфейса нет
    /// default-маршрута).
    Mismatch(String),
    /// Не удалось получить таблицу default-маршрутов.
    Source(RouteOpError),
}

/// Сверщик шлюзов с кэшем таблицы по семействам ([`GATEWAY_CACHE_TTL`]):
/// старт N бутылок подряд бьёт в ОС одним Get-NetRoute на семейство.
pub struct GatewayChecker {
    /// Источник таблицы.
    source: std::sync::Arc<dyn DefaultRouteSource>,
    /// Время жизни кэша (тесты удлиняют).
    ttl: Duration,
    /// Кэш по семействам: (момент, таблица).
    cache: tokio::sync::Mutex<GatewayTableCache>,
}

/// Слоты кэша таблиц по семействам.
#[derive(Default)]
struct GatewayTableCache {
    /// Таблица default-маршрутов IPv4.
    v4: Option<(std::time::Instant, Vec<DefaultRouteRow>)>,
    /// Таблица default-маршрутов IPv6.
    v6: Option<(std::time::Instant, Vec<DefaultRouteRow>)>,
}

impl GatewayChecker {
    /// Продакшн-сверщик: [`RealDefaultRoutes`] + [`GATEWAY_CACHE_TTL`].
    pub fn real() -> Self {
        Self::new(std::sync::Arc::new(RealDefaultRoutes), GATEWAY_CACHE_TTL)
    }

    /// Сверщик с явным источником и TTL (тесты).
    pub fn new(source: std::sync::Arc<dyn DefaultRouteSource>, ttl: Duration) -> Self {
        Self {
            source,
            ttl,
            cache: tokio::sync::Mutex::new(GatewayTableCache::default()),
        }
    }

    /// Сверяет «шлюз == текущий default next-hop интерфейса» (спека
    /// §4.2): таблица — из кэша или источника.
    pub async fn verify(
        &self,
        gateway: IpAddr,
        iface_alias: &str,
    ) -> Result<(), GatewayVerifyError> {
        let table = self
            .table(gateway.is_ipv6())
            .await
            .map_err(GatewayVerifyError::Source)?;
        check_gateway_matches(gateway, iface_alias, &table).map_err(GatewayVerifyError::Mismatch)
    }

    /// Таблица семейства с кэшированием на TTL.
    async fn table(&self, v6: bool) -> Result<Vec<DefaultRouteRow>, RouteOpError> {
        let mut cache = self.cache.lock().await;
        let slot = if v6 { &mut cache.v6 } else { &mut cache.v4 };
        if let Some((fetched_at, rows)) = slot {
            if fetched_at.elapsed() < self.ttl {
                return Ok(rows.clone());
            }
        }
        let rows = self.source.default_routes(v6).await?;
        *slot = Some((std::time::Instant::now(), rows.clone()));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    // ---------------------------------------------------------------
    // Полная валидация шлюза (спека §4.2).
    // ---------------------------------------------------------------

    fn table() -> Vec<DefaultRouteRow> {
        vec![
            DefaultRouteRow {
                iface_alias: "Ethernet".to_string(),
                next_hop: ip("192.168.1.1"),
            },
            DefaultRouteRow {
                iface_alias: "Wi-Fi".to_string(),
                next_hop: ip("10.0.0.138"),
            },
            DefaultRouteRow {
                iface_alias: "Ethernet".to_string(),
                next_hop: ip("2001:db8::1"),
            },
        ]
    }

    /// Разбор CSV от ConvertTo-Csv: заголовок, данные, пустые NextHop
    /// (on-link default без шлюза) пропускаются.
    #[test]
    fn parse_default_routes_csv_fixture() {
        let csv = "\"InterfaceAlias\",\"NextHop\"\r\n\"Ethernet\",\"192.168.1.1\"\r\n\"vEthernet (WSL)\",\"172.20.16.161\",\"\"\r\n\"vEthernet (WSL)\",\"172.20.16.1\"";
        let rows = parse_default_routes(csv);
        assert_eq!(
            rows,
            vec![
                DefaultRouteRow {
                    iface_alias: "Ethernet".to_string(),
                    next_hop: ip("192.168.1.1"),
                },
                DefaultRouteRow {
                    iface_alias: "vEthernet (WSL)".to_string(),
                    next_hop: ip("172.20.16.1"),
                },
            ]
        );
    }

    /// Шлюз совпадает с default next-hop интерфейса того же семейства.
    #[test]
    fn gateway_matches_iface_default_ok() {
        assert_eq!(
            check_gateway_matches(ip("192.168.1.1"), "Ethernet", &table()),
            Ok(())
        );
        assert_eq!(
            check_gateway_matches(ip("2001:db8::1"), "Ethernet", &table()),
            Ok(())
        );
    }

    /// Шлюз чужой (не default этого интерфейса) — отказ.
    #[test]
    fn gateway_of_other_iface_is_refused() {
        let error = check_gateway_matches(ip("10.0.0.138"), "Ethernet", &table())
            .expect_err("шлюз Wi-Fi не для Ethernet");
        assert!(
            error.contains("не соответствует интерфейсу"),
            "текст спеки: {error}"
        );
    }

    /// У интерфейса нет default-маршрута — тот же отказ.
    #[test]
    fn iface_without_default_route_is_refused() {
        let error = check_gateway_matches(ip("192.168.1.1"), "Vpn", &table())
            .expect_err("интерфейса нет в таблице");
        assert!(
            error.contains("не соответствует интерфейсу"),
            "единый текст для отсутствующего default: {error}"
        );
    }

    fn strs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn add_v4_full() {
        assert_eq!(
            route_ps_args(
                RouteCmd::Add,
                ip("203.0.113.77"),
                Some(ip("192.168.1.1")),
                Some("Ethernet"),
            ),
            strs(&[
                "New-NetRoute",
                "-DestinationPrefix",
                "'203.0.113.77/32'",
                "-NextHop",
                "'192.168.1.1'",
                "-InterfaceAlias",
                "'Ethernet'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
            ])
        );
    }

    #[test]
    fn add_v6_full_uses_slash_128() {
        assert_eq!(
            route_ps_args(
                RouteCmd::Add,
                ip("2001:db8::10"),
                Some(ip("fd00::1")),
                Some("vEthernet (WSL)"),
            ),
            strs(&[
                "New-NetRoute",
                "-DestinationPrefix",
                "'2001:db8::10/128'",
                "-NextHop",
                "'fd00::1'",
                "-InterfaceAlias",
                "'vEthernet (WSL)'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
            ])
        );
    }

    #[test]
    fn add_without_gateway_and_alias_omits_pairs() {
        assert_eq!(
            route_ps_args(RouteCmd::Add, ip("203.0.113.77"), None, None),
            strs(&[
                "New-NetRoute",
                "-DestinationPrefix",
                "'203.0.113.77/32'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
            ])
        );
    }

    #[test]
    fn add_with_gateway_only_or_alias_only() {
        assert_eq!(
            route_ps_args(RouteCmd::Add, ip("10.1.2.3"), Some(ip("10.0.0.1")), None),
            strs(&[
                "New-NetRoute",
                "-DestinationPrefix",
                "'10.1.2.3/32'",
                "-NextHop",
                "'10.0.0.1'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
            ])
        );
        assert_eq!(
            route_ps_args(RouteCmd::Add, ip("10.1.2.3"), None, Some("Wi-Fi")),
            strs(&[
                "New-NetRoute",
                "-DestinationPrefix",
                "'10.1.2.3/32'",
                "-InterfaceAlias",
                "'Wi-Fi'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
            ])
        );
    }

    #[test]
    fn del_v4_full() {
        assert_eq!(
            route_ps_args(
                RouteCmd::Del,
                ip("203.0.113.77"),
                Some(ip("192.168.1.1")),
                Some("Ethernet"),
            ),
            strs(&[
                "Remove-NetRoute",
                "-DestinationPrefix",
                "'203.0.113.77/32'",
                "-NextHop",
                "'192.168.1.1'",
                "-InterfaceAlias",
                "'Ethernet'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
                "-Confirm:$false",
            ])
        );
    }

    #[test]
    fn del_v6_minimal_uses_slash_128() {
        assert_eq!(
            route_ps_args(RouteCmd::Del, ip("2001:db8::10"), None, None),
            strs(&[
                "Remove-NetRoute",
                "-DestinationPrefix",
                "'2001:db8::10/128'",
                "-PolicyStore",
                "ActiveStore",
                "-ErrorAction",
                "Stop",
                "-Confirm:$false",
            ])
        );
    }

    #[test]
    fn list_v4_and_v6_are_read_only_prefix_queries() {
        assert_eq!(
            route_ps_args(RouteCmd::List, ip("203.0.113.77"), None, None),
            strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "'203.0.113.77/32'",
                "-ErrorAction",
                "SilentlyContinue",
            ])
        );
        assert_eq!(
            route_ps_args(RouteCmd::List, ip("2001:db8::10"), None, None),
            strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "'2001:db8::10/128'",
                "-ErrorAction",
                "SilentlyContinue",
            ])
        );
    }

    #[test]
    fn list_with_gateway_and_alias_keeps_query_shape() {
        assert_eq!(
            route_ps_args(
                RouteCmd::List,
                ip("203.0.113.77"),
                Some(ip("192.168.1.1")),
                Some("Ethernet"),
            ),
            strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "'203.0.113.77/32'",
                "-NextHop",
                "'192.168.1.1'",
                "-InterfaceAlias",
                "'Ethernet'",
                "-ErrorAction",
                "SilentlyContinue",
            ])
        );
    }

    /// Пин безопасности: метасимволы PowerShell внутри значений
    /// (враждебное имя интерфейса) обязаны оказаться полностью внутри
    /// PS-одинарных кавычек — вне кавычек `|`/`;`/backtick/`$` в
    /// векторе аргументов недопустимы.
    #[test]
    fn adversarial_alias_is_fully_quoted_literal() {
        let hostile = "Ethernet|calc; rm -rf C:\\ 'oops'`n$(whoami)";
        let args = route_ps_args(
            RouteCmd::Add,
            ip("203.0.113.77"),
            Some(ip("192.168.1.1")),
            Some(hostile),
        );
        // Точное значение-литерал: одинарные кавычки внутри удвоены,
        // всё остальное — как есть, внутри одной пары кавычек.
        let expected_alias = "'Ethernet|calc; rm -rf C:\\ ''oops''`n$(whoami)'".to_string();
        assert!(
            args.contains(&expected_alias),
            "алиас не заквочен: {args:?}"
        );
        // Токен сразу после флага значения обязан быть PS-литералом в
        // кавычках (контролируемые enum-литералы ActiveStore/Stop/…
        // идут в других позициях и голыми — это осознанно).
        let value_flags = ["-DestinationPrefix", "-NextHop", "-InterfaceAlias"];
        for (i, token) in args.iter().enumerate() {
            if value_flags.contains(&token.as_str()) {
                let value = &args[i + 1];
                assert!(
                    value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2,
                    "значение {value:?} после {token} не заквочено"
                );
            }
        }
        // Голые метасимволы вне кавычек (в флагах/литералах их нет):
        for token in &args {
            if !(token.starts_with('\'') && token.ends_with('\'')) {
                assert!(
                    !token.contains('|') && !token.contains(';') && !token.contains('`'),
                    "метасимвол вне кавычек: {token:?}"
                );
            }
        }
    }

    /// Флаги остаются голыми: квотированный `-Confirm:$false` перестал бы
    /// быть переключателем (первоначальный дефект раунда 1).
    #[test]
    fn flags_stay_unquoted() {
        let args = route_ps_args(RouteCmd::Del, ip("203.0.113.77"), None, None);
        assert_eq!(args.last().unwrap(), "-Confirm:$false");
        assert!(args.iter().any(|a| a == "Stop"));
        assert!(!args.iter().any(|a| a.contains("'Stop'")));
    }

    #[test]
    fn encode_ps_command_known_vector_utf16le_base64() {
        // Эталон посчитан независимо: python3 -c "base64.b64encode(
        // 'Write-Output привет'.encode('utf-16-le'))".
        assert_eq!(
            encode_ps_command(&strs(&["Write-Output", "привет"])),
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIAA/BEAEOAQyBDUEQgQ="
        );
    }

    #[test]
    fn encode_ps_command_route_line_vector() {
        // Эталон: base64('Get-NetRoute -DestinationPrefix
        // 203.0.113.77/32' в UTF-16LE), посчитан независимо.
        assert_eq!(
            encode_ps_command(&strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "203.0.113.77/32"
            ])),
            concat!(
                "RwBlAHQALQBOAGUAdABSAG8AdQB0AGUAIAAtAEQAZQBzAHQAaQBuAGEAdABpAG8A",
                "bgBQAHIAZQBmAGkAeAAgADIAMAAzAC4AMAAuADEAMQAzAC4ANwA3AC8AMwAyAA=="
            )
        );
    }

    #[test]
    fn base64_matches_rfc4648_test_vectors() {
        // Эталоны RFC 4648 §10 (базовый алфавит, с паддингом) — на
        // самих байтах; UTF-16LE-склейку проверяют эталоны
        // encode_ps_command выше.
        for (text, expected) in [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(text.as_bytes()), expected, "вектор {text:?}");
        }
    }
}
