//! Чистый рендер ответов демона в пользовательский вывод на русском.
//!
//! Функции не знают о транспорте и работают на любой платформе — вывод
//! CLI покрыт unit-тестами и на не-Windows хостах.

use bottle_core::ipc::{BottleState, BottleSummary, RuntimeState};
use bottle_core::profile::{BottleKind, BottleProfile, TunnelProtocol, VpnConfig};

/// Ширина колонки меток в `show` (по самой длинной «expose:»).
const LABEL_WIDTH: usize = 8;

/// Ширина колонки kind в `list` (по «host» и заголовку «kind»).
const KIND_WIDTH: usize = 4;

/// Рендерит ответ `List`: таблица «id  name  kind  state» колонками;
/// id выровнен по самому длинному, неизвестное состояние — «-».
pub fn render_list(items: &[BottleSummary]) -> String {
    if items.is_empty() {
        return "Бутылок нет".to_string();
    }

    let id_width = items
        .iter()
        .map(|item| item.id.chars().count())
        .max()
        .unwrap_or(0)
        .max("id".len());
    let name_width = items
        .iter()
        .map(|item| item.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("name".len());

    let mut lines = vec![format!(
        "{:<id_width$}  {:<name_width$}  {:<KIND_WIDTH$}  state",
        "id", "name", "kind",
    )];
    for item in items {
        lines.push(format!(
            "{:<id_width$}  {:<name_width$}  {:<KIND_WIDTH$}  {}",
            item.id,
            item.name,
            kind_str(item.kind),
            item.state.as_deref().unwrap_or("-"),
        ));
    }
    lines.join("\n")
}

/// Рендерит ответ `Status`: блок на бутылку — «id  состояние» и
/// назначенные порты (SOCKS5, пробросы) строками ниже.
pub fn render_status(items: &[RuntimeState]) -> String {
    if items.is_empty() {
        return "Нет поднятых бутылок".to_string();
    }
    items
        .iter()
        .map(runtime_lines)
        .map(|lines| lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Рендерит ответ `Show`: профиль (id/name/kind/vpn-сводка + expose)
/// и рантайм-состояние, если оно есть.
pub fn render_show(profile: &BottleProfile, runtime: Option<&RuntimeState>) -> String {
    let mut lines = vec![
        format!("{:<LABEL_WIDTH$}{}", "id:", profile.id),
        format!("{:<LABEL_WIDTH$}{}", "name:", profile.name),
        format!("{:<LABEL_WIDTH$}{}", "kind:", kind_str(profile.kind)),
        format!("{:<LABEL_WIDTH$}{}", "vpn:", vpn_summary(&profile.vpn)),
        "expose:".to_string(),
        format!("  socks5: {}", port_or_auto(profile.expose.socks5)),
    ];
    if !profile.expose.forwards.is_empty() {
        lines.push("  forwards:".to_string());
        for forward in &profile.expose.forwards {
            lines.push(format!(
                "    {} → 127.0.0.1:{}",
                forward.target,
                port_or_auto(forward.listen)
            ));
        }
    }
    if let Some(dns) = profile.expose.dns {
        lines.push(format!("  dns: {dns}"));
    }
    if let Some(launch) = &profile.expose.launch {
        lines.push(format!("  launch: {launch}"));
    }
    if let Some(runtime) = runtime {
        lines.push("runtime:".to_string());
        lines.extend(
            runtime_lines(runtime)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    lines.join("\n")
}

/// Строки рантайм-блока (общий для `status` и секции `runtime:` в `show`):
/// «id  состояние», затем назначенные порты.
fn runtime_lines(runtime: &RuntimeState) -> Vec<String> {
    let mut lines = vec![format!("{}  {}", runtime.id, state_str(&runtime.state))];
    if let Some(port) = runtime.socks5 {
        lines.push(format!("  socks5 127.0.0.1:{port}"));
    }
    for forward in &runtime.forwards {
        lines.push(format!(
            "  {} {} → 127.0.0.1:{}",
            forward_label(&forward.target),
            forward.target,
            forward.port,
        ));
    }
    lines
}

/// Состояние каноническим именем; `failed` и `degraded` дополнительно
/// печатают причину.
fn state_str(state: &BottleState) -> String {
    match state {
        BottleState::Failed { reason } => format!("failed ({reason})"),
        BottleState::Degraded { reason } => format!("degraded ({reason})"),
        other => other.as_str().to_string(),
    }
}

/// Краткая сводка `[vpn]`: env — протокол и конфиг, host — клиент и endpoints.
fn vpn_summary(vpn: &VpnConfig) -> String {
    match vpn {
        VpnConfig::Env { protocol, config } => {
            format!("{}, config: {}", protocol_str(*protocol), config)
        }
        VpnConfig::Host {
            client, endpoints, ..
        } => format!("{client}, endpoints: {}", endpoints.join(", ")),
    }
}

/// Порт из профиля: 0 означает автоподбор при старте.
fn port_or_auto(port: u16) -> String {
    if port == 0 {
        "авто".to_string()
    } else {
        port.to_string()
    }
}

/// Каноническое имя типа бутылки (совпадает с форматом профиля).
fn kind_str(kind: BottleKind) -> &'static str {
    match kind {
        BottleKind::Env => "env",
        BottleKind::Host => "host",
    }
}

/// Каноническое имя протокола туннеля (совпадает с форматом профиля).
fn protocol_str(protocol: TunnelProtocol) -> &'static str {
    match protocol {
        TunnelProtocol::WireGuard => "wireguard",
        TunnelProtocol::OpenVpn => "openvpn",
    }
}

/// Метка проброса по порту цели: известные службы — по имени, прочие — «tcp».
fn forward_label(target: &str) -> &'static str {
    let target_port = target
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok());
    match target_port {
        Some(3389) => "rdp",
        _ => "tcp",
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec};
    use bottle_core::protocol::ForwardPort;

    /// Env-профиль из спеки §5.1: socks5 и listen = 0 (автоподбор).
    fn env_profile() -> BottleProfile {
        BottleProfile {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
            },
            expose: ExposeConfig {
                socks5: 0,
                forwards: vec![ForwardSpec {
                    listen: 0,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some(IpAddr::from([10, 20, 30, 1])),
                launch: Some("mstsc".to_string()),
            },
        }
    }

    /// Host-профиль: фиксированный socks5, без пробросов/dns/launch.
    fn host_profile() -> BottleProfile {
        BottleProfile {
            id: "corp-vpn".to_string(),
            name: "Корп VPN".to_string(),
            kind: BottleKind::Host,
            vpn: VpnConfig::Host {
                client: r"C:\Program Files\Vendor\vpn.exe".to_string(),
                args: Some("connect office".to_string()),
                endpoints: vec!["vpn.example.com".to_string()],
                interface: Some("Vendor Adapter".to_string()),
            },
            expose: ExposeConfig {
                socks5: 1080,
                forwards: Vec::new(),
                dns: None,
                launch: None,
            },
        }
    }

    /// Рантайм из брифа Task 17: up, SOCKS5 55001, проброс RDP на 55002.
    fn runtime(state: BottleState) -> RuntimeState {
        RuntimeState {
            id: "office-a".to_string(),
            state,
            socks5: Some(55001),
            forwards: vec![ForwardPort {
                port: 55002,
                target: "10.20.30.15:3389".to_string(),
            }],
        }
    }

    // Бриф Task 17 Step 1: обязательные подстроки ответа Status.
    #[test]
    fn status_brief_contains_id_state_socks5_and_forward() {
        let out = render_status(&[runtime(BottleState::Up)]);
        assert!(out.contains("office-a"), "нет id:\n{out}");
        assert!(out.contains("up"), "нет состояния:\n{out}");
        assert!(out.contains("socks5 127.0.0.1:55001"), "нет socks5:\n{out}");
        assert!(
            out.contains("rdp 10.20.30.15:3389 → 127.0.0.1:55002"),
            "нет проброса:\n{out}"
        );
    }

    #[test]
    fn status_failed_prints_reason() {
        let out = render_status(&[runtime(BottleState::Failed {
            reason: "туннель не поднялся".to_string(),
        })]);
        assert!(out.contains("failed"), "нет состояния:\n{out}");
        assert!(out.contains("туннель не поднялся"), "нет причины:\n{out}");
    }

    /// T11: деградация видна с причиной (агент недоступен / умер релей).
    #[test]
    fn status_degraded_prints_reason() {
        let out = render_status(&[runtime(BottleState::Degraded {
            reason: "агент аплаенса не отвечает".to_string(),
        })]);
        assert!(out.contains("degraded"), "нет состояния:\n{out}");
        assert!(
            out.contains("агент аплаенса не отвечает"),
            "нет причины:\n{out}"
        );
    }

    // Бриф Task 17 Step 1: state None отображается как «-».
    #[test]
    fn list_shows_dash_for_unknown_state_and_aligns_ids() {
        let out = render_list(&[
            BottleSummary {
                id: "a".to_string(),
                name: "Офис А".to_string(),
                kind: BottleKind::Env,
                state: None,
            },
            BottleSummary {
                id: "office-a".to_string(),
                name: "Корп VPN".to_string(),
                kind: BottleKind::Host,
                state: Some("up".to_string()),
            },
        ]);

        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 3, "заголовок + две строки:\n{out}");
        assert!(rows[0].starts_with("id"), "нет заголовка:\n{out}");
        assert!(rows[1].contains(" env "), "нет kind env:\n{out}");
        assert!(rows[1].contains(" -"), "нет «-» для state None:\n{out}");
        assert!(rows[2].contains(" host "), "нет kind host:\n{out}");
        assert!(rows[2].contains(" up"), "нет состояния up:\n{out}");
        // id выровнены по самому длинному: имена в одной колонке.
        assert_eq!(
            rows[1].find("Офис А"),
            rows[2].find("Корп VPN"),
            "колонка name не выровнена:\n{out}"
        );
    }

    #[test]
    fn empty_list_and_status_have_russian_messages() {
        assert_eq!(render_list(&[]), "Бутылок нет");
        assert_eq!(render_status(&[]), "Нет поднятых бутылок");
    }

    #[test]
    fn show_env_profile_renders_vpn_and_expose() {
        let out = render_show(&env_profile(), None);
        for expected in [
            "office-a",
            "Офис А",
            "env",
            "wireguard",
            "wg-office-a.conf",
            "socks5: авто",
            "10.20.30.15:3389",
            "dns: 10.20.30.1",
            "launch: mstsc",
        ] {
            assert!(out.contains(expected), "нет «{expected}» в:\n{out}");
        }
        assert!(
            !out.contains("runtime:"),
            "без рантайма секции быть не должно:\n{out}"
        );
    }

    #[test]
    fn show_host_profile_renders_client_and_endpoints() {
        let out = render_show(&host_profile(), None);
        for expected in [
            "corp-vpn",
            "host",
            r"C:\Program Files\Vendor\vpn.exe",
            "vpn.example.com",
            "socks5: 1080",
        ] {
            assert!(out.contains(expected), "нет «{expected}» в:\n{out}");
        }
    }

    #[test]
    fn show_with_runtime_renders_live_ports() {
        let out = render_show(&env_profile(), Some(&runtime(BottleState::Up)));
        assert!(out.contains("runtime:"), "нет секции рантайма:\n{out}");
        assert!(
            out.contains("socks5 127.0.0.1:55001"),
            "нет живого socks5:\n{out}"
        );
        assert!(
            out.contains("rdp 10.20.30.15:3389 → 127.0.0.1:55002"),
            "нет живого проброса:\n{out}"
        );
    }

    #[test]
    fn non_rdp_forward_is_tcp_and_missing_socks5_is_omitted() {
        let out = render_status(&[RuntimeState {
            id: "db".to_string(),
            state: BottleState::Up,
            socks5: None,
            forwards: vec![ForwardPort {
                port: 15432,
                target: "10.0.0.5:5432".to_string(),
            }],
        }]);
        assert!(
            out.contains("tcp 10.0.0.5:5432 → 127.0.0.1:15432"),
            "нет генерического проброса:\n{out}"
        );
        assert!(!out.contains("socks5"), "нет порта — нет строки:\n{out}");
    }
}
