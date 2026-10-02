//! Операции над таблицей маршрутов ОС (Windows) через командлеты
//! NetTCPIP: `New-NetRoute`/`Remove-NetRoute`/`Get-NetRoute` с
//! `-PolicyStore ActiveStore` — маршруты живут до перезагрузки,
//! живучесть обеспечивается сверкой при старте службы (задача 5).
//!
//! Единая точка построения точечных префиксов — [`route_ps_args`]:
//! `/32` для IPv4, `/128` для IPv6 (чистая функция, unit-тестируется
//! на любой ОС); фактический запуск PowerShell — [`run_ps`].

use std::net::IpAddr;

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

    /// Команда модифицирует таблицу маршрутов (не `List`).
    fn is_write(self) -> bool {
        !matches!(self, RouteCmd::List)
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

/// Собирает аргументы PowerShell-командлета для операции над маршрутом.
///
/// Общее начало: командлет + `-DestinationPrefix <ip>/32|/128`; при
/// заданном `gateway` добавляется пара `-NextHop <gw>`, при заданном
/// `iface_alias` — пара `-InterfaceAlias <alias>`. `Add`/`Del` пишут
/// только в активное хранилище (`-PolicyStore ActiveStore`) и падают
/// при сбое (`-ErrorAction Stop`); `Del` дополнительно отключает
/// запрос подтверждения (`-Confirm:$false`). `List` — читающий запрос
/// по префиксу назначения.
pub fn route_ps_args(
    cmd: RouteCmd,
    dest: IpAddr,
    gateway: Option<IpAddr>,
    iface_alias: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        cmd.cmdlet().to_string(),
        "-DestinationPrefix".to_string(),
        host_prefix(dest),
    ];
    if let Some(gateway) = gateway {
        args.push("-NextHop".to_string());
        args.push(gateway.to_string());
    }
    if let Some(alias) = iface_alias {
        args.push("-InterfaceAlias".to_string());
        args.push(alias.to_string());
    }
    if cmd.is_write() {
        args.push("-PolicyStore".to_string());
        args.push("ActiveStore".to_string());
        args.push("-ErrorAction".to_string());
        args.push("Stop".to_string());
    }
    if matches!(cmd, RouteCmd::Del) {
        args.push("-Confirm:$false".to_string());
    }
    args
}

/// Собирает аргументы в командную строку для PowerShell `-Command`,
/// заключая значения с пробелами и спецсимволами в литеральные
/// одиночные кавычки (вложенный `'` удваивается): алиасы адаптеров
/// вроде `vEthernet (WSL)` содержат пробелы и скобки.
// На не-windows вызывается только windows-реализацией run_ps и
// unit-тестами экранирования (паттерн daemon).
#[cfg_attr(not(windows), allow(dead_code))]
fn ps_command_line(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_ps_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Экранирует один аргумент PowerShell при необходимости.
#[cfg_attr(not(windows), allow(dead_code))]
fn quote_ps_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty() || arg.chars().any(ps_special);
    if needs_quoting {
        format!("'{}'", arg.replace('\'', "''"))
    } else {
        arg.to_string()
    }
}

/// Символы, из-за которых значение аргумента берётся в кавычки:
/// пробельные, группировка `( )`, кавычки, подстановка `$`, разделитель
/// `;`, запятая и escape-символ PowerShell — backtick.
#[cfg_attr(not(windows), allow(dead_code))]
fn ps_special(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '\'' | '"' | '$' | ';' | ',' | '`')
}

/// Запускает PowerShell с собранными аргументами и возвращает stdout.
///
/// `powershell -NoProfile -NonInteractive -Command <join(args)>`;
/// ненулевой код выхода → [`RouteOpError::PsFailed`] со stderr.
#[cfg(windows)]
pub fn run_ps(args: &[String]) -> Result<String, RouteOpError> {
    let output = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps_command_line(args))
        .output()
        .map_err(RouteOpError::Spawn)?;
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
pub fn run_ps(_args: &[String]) -> Result<String, RouteOpError> {
    Err(RouteOpError::NotWindows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
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
                "203.0.113.77/32",
                "-NextHop",
                "192.168.1.1",
                "-InterfaceAlias",
                "Ethernet",
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
                "2001:db8::10/128",
                "-NextHop",
                "fd00::1",
                "-InterfaceAlias",
                "vEthernet (WSL)",
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
                "203.0.113.77/32",
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
                "10.1.2.3/32",
                "-NextHop",
                "10.0.0.1",
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
                "10.1.2.3/32",
                "-InterfaceAlias",
                "Wi-Fi",
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
                "203.0.113.77/32",
                "-NextHop",
                "192.168.1.1",
                "-InterfaceAlias",
                "Ethernet",
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
                "2001:db8::10/128",
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
            strs(&["Get-NetRoute", "-DestinationPrefix", "203.0.113.77/32"])
        );
        assert_eq!(
            route_ps_args(RouteCmd::List, ip("2001:db8::10"), None, None),
            strs(&["Get-NetRoute", "-DestinationPrefix", "2001:db8::10/128"])
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
                "203.0.113.77/32",
                "-NextHop",
                "192.168.1.1",
                "-InterfaceAlias",
                "Ethernet",
            ])
        );
    }

    #[test]
    fn command_line_quotes_only_special_values() {
        assert_eq!(
            ps_command_line(&strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "203.0.113.7/32"
            ])),
            "Get-NetRoute -DestinationPrefix 203.0.113.7/32"
        );
        // Пробелы и скобки в алиасе адаптера — литеральная строка PS.
        assert_eq!(
            ps_command_line(&strs(&["-InterfaceAlias", "vEthernet (WSL)"])),
            "-InterfaceAlias 'vEthernet (WSL)'"
        );
        // Вложенная одинарная кавычка удваивается.
        assert_eq!(ps_command_line(&strs(&["it's", "a'b"])), "'it''s' 'a''b'");
    }
}
