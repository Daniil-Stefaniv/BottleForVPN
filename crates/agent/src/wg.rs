//! WireGuard-туннель внутри namespace бутылки (задача 10).
//!
//! [`parse_wg_config`] разбирает `.conf` wg-quick: `Address` и `Endpoint`
//! выносятся в поля [`ParsedWg`] (endpoint может быть DNS-именем —
//! `wg setconf` требует IP, поэтому резолвится в namespace через
//! [`resolve_endpoint_in_ns`]), остальное собирается в тело `wg setconf`.
//!
//! [`tunnel_up_wg`] поднимает userspace-реализацию `wireguard-go` внутри
//! namespace, настраивает wg-интерфейс (имя `wg<idx>` — уникально per
//! слот, см. [`wg_iface_name`]), маршруты `0.0.0.0/1` + `128.0.0.0/1`
//! (перекрывают default-маршрут, не ломая маршрут до самого endpoint)
//! и ждёт первый handshake; [`tunnel_down`] останавливает процесс.

use std::io::Write as _;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::AgentError;
use crate::netns::{run_cmd, run_in_ns, NetnsHandle};

/// Имя wg-интерфейса бутылки на слоте `idx`: `wg<idx>`.
///
/// Имя обязано быть уникальным среди одновременно поднятых бутылок:
/// UAPI-сокет wireguard-go живёт в `/var/run/wireguard/<iface>.sock`
/// — файловая система общая для всех network namespaces, и две бутылки
/// с одинаковым именем интерфейса подрались бы за один сокет
/// (второй `wireguard-go` не стартует, `wg show` уходил бы в чужой
/// интерфейс).
pub fn wg_iface_name(idx: u8) -> String {
    format!("wg{idx}")
}
/// Предел ожидания первого handshake.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(30);
/// Пауза между опросами `wg show ... latest-handshakes`.
const HANDSHAKE_POLL: Duration = Duration::from_secs(1);
/// Предел ожидания готовности wireguard-go (интерфейс + UAPI-сокет).
const WG_READY_WAIT: Duration = Duration::from_secs(5);
/// Пауза между опросами готовности wireguard-go.
const WG_READY_POLL: Duration = Duration::from_millis(100);

/// Разобранный конфиг WireGuard (подмножество формата wg-quick).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWg {
    /// Адрес из `[Interface] Address` без маски (`10.8.0.2` из `10.8.0.2/32`).
    pub interface_addr: Ipv4Addr,
    /// Хост из `[Peer] Endpoint` (DNS-имя или IP) — в `setconf_body`
    /// не попадает: `wg setconf` требует IP-адрес, имя резолвится
    /// в namespace через [`resolve_endpoint_in_ns`].
    pub endpoint_host: Option<String>,
    /// Порт из `[Peer] Endpoint`.
    pub endpoint_port: u16,
    /// Тело `wg setconf` без Endpoint: `[Interface] PrivateKey`,
    /// `ListenPort?`; `[Peer] PublicKey`, `PresharedKey?`, `AllowedIPs`,
    /// `PersistentKeepalive?`.
    pub setconf_body: String,
}

/// Пара «ключ = значение» из секции конфига.
type Entry = (String, String);

/// Разбирает текст `.conf` WireGuard (формат wg-quick).
///
/// Поддерживается один пир: конфиг с несколькими секциями `[Peer]`
/// отвергается явной ошибкой (без барьера ключи пиров молча
/// сливались в один кусок `wg setconf` и падали невнятным отказом
/// wg на дубликатах). Секции `[Interface]`/`[Peer]`, строки
/// «ключ = значение», комментарии `#`; посторонние для ядра ключи
/// wg-quick (`DNS`, `Table`, `MTU`, …) игнорируются. Обязательные ключи:
/// `Address` и `PrivateKey` в `[Interface]`; `PublicKey`, `AllowedIPs`
/// и `Endpoint` в `[Peer]` — отсутствие или нераспознанная строка дают
/// [`AgentError::InvalidConfig`].
pub fn parse_wg_config(text: &str) -> Result<ParsedWg, AgentError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        None,
        Interface,
        Peer,
    }

    let mut section = Section::None;
    let (mut iface, mut peer): (Vec<Entry>, Vec<Entry>) = (Vec::new(), Vec::new());
    let mut peer_sections = 0usize;

    for (num, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            section = match name {
                "Interface]" => Section::Interface,
                "Peer]" => {
                    peer_sections += 1;
                    if peer_sections > 1 {
                        return Err(invalid_config(format!(
                            "строка {}: поддерживается один пир, секций [Peer] несколько",
                            num + 1
                        )));
                    }
                    Section::Peer
                }
                _ => {
                    // Первый токен — имя секции в скобках; значение секции
                    // не бывает секретом, а остаток строки в ошибку не
                    // попадает.
                    let section_token = line.split_whitespace().next().unwrap_or("[]").to_string();
                    return Err(invalid_config(format!(
                        "строка {}: неизвестная секция {section_token}",
                        num + 1
                    )));
                }
            };
            continue;
        }
        // Строка без «=» не позволяет отделить ключ от значения: сырую
        // строку в ошибку не подставляем — там может оказаться PrivateKey.
        let (key, value) = line.split_once('=').ok_or_else(|| {
            invalid_config(format!("строка {}: ожидается «ключ = значение»", num + 1))
        })?;
        let key = key.trim().to_string();
        let entry = (key.clone(), value.trim().to_string());
        match section {
            Section::None => {
                // Имя ключа безопасно для диагностики, значение — нет.
                return Err(invalid_config(format!(
                    "строка {}: ключ \"{key}\" вне секции [Interface]/[Peer]",
                    num + 1
                )));
            }
            Section::Interface => iface.push(entry),
            Section::Peer => peer.push(entry),
        }
    }

    let find = |entries: &[Entry], key: &str| -> Option<String> {
        entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    let require = |entries: &[Entry], key: &str| -> Result<String, AgentError> {
        find(entries, key).ok_or_else(|| invalid_config(format!("нет обязательного ключа {key}")))
    };

    let address = require(&iface, "Address")?;
    let interface_addr = address
        .split('/')
        .next()
        .unwrap_or(&address)
        .parse()
        .map_err(|_| invalid_config(format!("Address не является IPv4-адресом: {address}")))?;
    require(&iface, "PrivateKey")?;
    require(&peer, "PublicKey")?;
    require(&peer, "AllowedIPs")?;

    let endpoint = require(&peer, "Endpoint")?;
    let (endpoint_host, endpoint_port) = endpoint
        .rsplit_once(':')
        .ok_or_else(|| invalid_config(format!("Endpoint без порта: {endpoint}")))?;
    let endpoint_port: u16 = endpoint_port
        .parse()
        .map_err(|_| invalid_config(format!("порт Endpoint не число: {endpoint}")))?;
    if endpoint_host.is_empty() {
        return Err(invalid_config(format!("Endpoint без хоста: {endpoint}")));
    }

    // Тело setconf: только ключи, понимаемые `wg`; порядок строк сохранён.
    let mut body = String::from("[Interface]\n");
    for (key, value) in &iface {
        if matches!(key.as_str(), "PrivateKey" | "ListenPort") {
            body.push_str(key);
            body.push_str(" = ");
            body.push_str(value);
            body.push('\n');
        }
    }
    body.push_str("\n[Peer]\n");
    for (key, value) in &peer {
        if matches!(
            key.as_str(),
            "PublicKey" | "PresharedKey" | "AllowedIPs" | "PersistentKeepalive"
        ) {
            body.push_str(key);
            body.push_str(" = ");
            body.push_str(value);
            body.push('\n');
        }
    }

    Ok(ParsedWg {
        interface_addr,
        endpoint_host: Some(endpoint_host.to_string()),
        endpoint_port,
        setconf_body: body,
    })
}

/// Разрешает хост endpoint в IPv4-адрес внутри namespace `ns`.
///
/// Dotted-IPv4 возвращается как есть; DNS-имя разрешается через
/// `ip netns exec <ns> getent ahostsv4 <host>` — то есть DNS-серверами
/// самого namespace (`/etc/netns/<ns>/resolv.conf`), а не хоста.
pub async fn resolve_endpoint_in_ns(ns: &str, host: &str) -> Result<Ipv4Addr, AgentError> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let output = run_cmd("ip", &["netns", "exec", ns, "getent", "ahostsv4", host])?;
    let resolved = output
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| {
            AgentError::TunnelFailed(format!("хост {host} не разрешается в namespace {ns}"))
        })?;
    resolved.parse().map_err(|_| {
        AgentError::TunnelFailed(format!(
            "getent вернул не IPv4-адрес для {host}: {resolved}"
        ))
    })
}

/// Поднимает WireGuard-туннель `wg<idx>` (слот бутылки) внутри её
/// namespace.
///
/// Порядок: резолв endpoint (DNS — внутри namespace); запуск
/// `wireguard-go wg<idx>` (долгоживущий процесс, сохраняется в
/// `h.tunnel_child`); link up + адрес `/32`; `wg setconf` телом
/// [`ParsedWg::setconf_body`] с дописанным `Endpoint = <ip>:<port>`;
/// маршрут до endpoint `/32` через peer-gateway слота (упакованный
/// трафик туннеля не уходит в сам туннель); маршруты `0.0.0.0/1` и
/// `128.0.0.0/1` через wg-интерфейс; ожидание первого handshake
/// (≤30 с, иначе [`AgentError::TunnelFailed`] — код `TUNNEL_FAILED`
/// у задачи 11).
pub async fn tunnel_up_wg(h: &mut NetnsHandle, cfg: ParsedWg) -> Result<(), AgentError> {
    let ns = h.ns_name.clone();
    let iface = wg_iface_name(h.slot.idx);

    let endpoint_host = cfg.endpoint_host.clone().ok_or_else(|| {
        AgentError::InvalidConfig("конфиг без Endpoint не может быть поднят как туннель".into())
    })?;
    let endpoint_ip = resolve_endpoint_in_ns(&ns, &endpoint_host).await?;

    let child = Command::new("ip")
        .args(["netns", "exec", &ns, "wireguard-go", &iface])
        // Без foreground wireguard-go демонизируется: прямой потомок
        // (`ip netns exec …`) сразу завершается, и wait_wg_ready
        // принимает это за падение туннеля. Плюс foreground глушит
        // stderr-баннер про kernel-поддержку WireGuard.
        .env("WG_PROCESS_FOREGROUND", "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    h.tunnel_child = Some(child);

    // wireguard-go создаёт интерфейс и UAPI-сокет асинхронно — ждём оба.
    wait_wg_ready(&ns, &iface, h).await?;
    run_in_ns(&ns, &["ip", "link", "set", &iface, "up"])?;
    run_in_ns(
        &ns,
        &[
            "ip",
            "addr",
            "add",
            &format!("{}/32", cfg.interface_addr),
            "dev",
            &iface,
        ],
    )?;

    let mut conf = cfg.setconf_body.clone();
    if !conf.is_empty() && !conf.ends_with('\n') {
        conf.push('\n');
    }
    conf.push_str("Endpoint = ");
    conf.push_str(&endpoint_ip.to_string());
    conf.push(':');
    conf.push_str(&cfg.endpoint_port.to_string());
    conf.push('\n');
    run_in_ns_stdin(&ns, &["wg", "setconf", &iface, "/dev/stdin"], &conf)?;

    run_in_ns(
        &ns,
        &[
            "ip",
            "route",
            "add",
            &format!("{endpoint_ip}/32"),
            "via",
            &h.slot.peer.to_string(),
        ],
    )?;
    run_in_ns(&ns, &["ip", "route", "add", "0.0.0.0/1", "dev", &iface])?;
    run_in_ns(&ns, &["ip", "route", "add", "128.0.0.0/1", "dev", &iface])?;

    wait_handshake(&ns, &iface, h).await
}

/// Останавливает туннель: завершает процесс wireguard-go из
/// `h.tunnel_child` (wg-интерфейс исчезает вместе с процессом).
///
/// Ошибки логируются, не паникуют; повторный вызов — no-op.
/// Маршруты и адрес удалять не нужно: они живут в namespace бутылки
/// и сносятся вместе с ним в `netns::netns_down`.
pub fn tunnel_down(h: &mut NetnsHandle) {
    if let Some(mut child) = h.tunnel_child.take() {
        if let Err(error) = child.kill() {
            tracing::warn!(ns = %h.ns_name, %error, "не удалось остановить wireguard-go");
        }
        let _ = child.wait();
    }
}

/// Ждёт готовности wireguard-go: `wg show` отвечает, когда существуют
/// и интерфейс, и UAPI-сокет. Ранний выход — если процесс умер.
async fn wait_wg_ready(ns: &str, iface: &str, h: &mut NetnsHandle) -> Result<(), AgentError> {
    let deadline = Instant::now() + WG_READY_WAIT;
    loop {
        if tunnel_child_dead(h) {
            return Err(AgentError::TunnelFailed(
                "процесс wireguard-go завершился при старте".into(),
            ));
        }
        if run_in_ns(ns, &["wg", "show", iface]).is_ok() {
            return Ok(());
        }
        if Instant::now() + WG_READY_POLL > deadline {
            return Err(AgentError::TunnelFailed(format!(
                "wireguard-go не готов за {} с (интерфейс {iface})",
                WG_READY_WAIT.as_secs()
            )));
        }
        tokio::time::sleep(WG_READY_POLL).await;
    }
}

/// Ждёт первый handshake: `wg show <iface> latest-handshakes` возвращает
/// unix-время последнего handshake (>0). Ранний выход — если процесс
/// wireguard-go умер.
async fn wait_handshake(ns: &str, iface: &str, h: &mut NetnsHandle) -> Result<(), AgentError> {
    let deadline = Instant::now() + HANDSHAKE_WAIT;
    loop {
        if let Ok(output) = run_in_ns(ns, &["wg", "show", iface, "latest-handshakes"]) {
            let ts = output
                .split_whitespace()
                .nth(1)
                .and_then(|token| token.parse::<u64>().ok());
            if ts.is_some_and(|ts| ts > 0) {
                return Ok(());
            }
        }
        if tunnel_child_dead(h) {
            return Err(AgentError::TunnelFailed(
                "процесс wireguard-go завершился до handshake".into(),
            ));
        }
        if Instant::now() + HANDSHAKE_POLL > deadline {
            return Err(AgentError::TunnelFailed(format!(
                "handshake {iface} не произошёл за {} с",
                HANDSHAKE_WAIT.as_secs()
            )));
        }
        tokio::time::sleep(HANDSHAKE_POLL).await;
    }
}

/// Завершился ли процесс туннеля (отсутствие процесса тоже «завершился»).
pub(crate) fn tunnel_child_dead(h: &mut NetnsHandle) -> bool {
    h.tunnel_child.as_mut().is_none_or(|child| {
        child
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(true)
    })
}

/// Выполняет команду внутри namespace, передав `input` в stdin.
fn run_in_ns_stdin(ns: &str, args: &[&str], input: &str) -> Result<(), AgentError> {
    let mut argv = vec!["netns", "exec", ns];
    argv.extend_from_slice(args);
    let mut child = Command::new("ip")
        .args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .expect("stdin запрограммирован как piped");
        stdin.write_all(input.as_bytes())?;
    } // drop закрывает канал: wg видит EOF конфигурации
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(AgentError::CommandFailed {
            command: format!("ip {}", argv.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Ошибка разбора конфига.
fn invalid_config(message: impl Into<String>) -> AgentError {
    AgentError::InvalidConfig(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Валидный приватный ключ (случайные 32 байта base64, только для теста).
    const PRIV: &str = "qverqEetmElQFhD2Sk3GBfsDSuJmiS+4dZx0KZ0I3Wc=";
    /// Соответствующий ему публичный ключ.
    const PUB: &str = "VQe6zyaC1LfEjdQaY5NjgtC6zShIB0j6iMe7H1t4ZxU=";
    /// Общий ключ пары (PresharedKey).
    const PSK: &str = "FpCyhaw9L4Bk9CVnTuxEjpvGCnVPaKtIWQCBiV6LsSg=";

    /// Фикстура полного конфига с подставляемым Endpoint.
    fn fixture(endpoint: &str) -> String {
        format!(
            "[Interface]\n\
             # комментарий wg-quick\n\
             Address = 10.8.0.2/32\n\
             PrivateKey = {PRIV}\n\
             ListenPort = 51820\n\
             \n\
             [Peer]\n\
             PublicKey = {PUB}\n\
             PresharedKey = {PSK}\n\
             AllowedIPs = 0.0.0.0/0, ::/0\n\
             Endpoint = {endpoint}\n\
             PersistentKeepalive = 25\n"
        )
    }

    #[test]
    fn dns_endpoint_fixture_parses() {
        let parsed = parse_wg_config(&fixture("vpn.example.com:51820")).expect("валидный конфиг");

        assert_eq!(parsed.interface_addr, Ipv4Addr::new(10, 8, 0, 2));
        assert_eq!(
            parsed.endpoint_host.as_deref(),
            Some("vpn.example.com"),
            "DNS-хост выносится в endpoint_host"
        );
        assert_eq!(parsed.endpoint_port, 51820);

        // setconf: Address и Endpoint извлечены, остальное сохранено как есть.
        let body = &parsed.setconf_body;
        assert!(
            body.starts_with("[Interface]\n"),
            "тело начинается с [Interface]: {body}"
        );
        assert!(
            !body.contains("Endpoint"),
            "Endpoint не должен попадать в setconf: {body}"
        );
        assert!(
            !body.contains("Address"),
            "Address не должен попадать в setconf: {body}"
        );
        assert!(body.contains("[Peer]\n"), "нет секции [Peer]: {body}");
        assert!(
            body.contains("AllowedIPs = 0.0.0.0/0, ::/0"),
            "AllowedIPs сохранены дословно: {body}"
        );
        assert!(
            body.contains(&format!("PrivateKey = {PRIV}")),
            "нет PrivateKey: {body}"
        );
        assert!(
            body.contains(&format!("PublicKey = {PUB}")),
            "нет PublicKey: {body}"
        );
        assert!(
            body.contains(&format!("PresharedKey = {PSK}")),
            "нет PresharedKey: {body}"
        );
        assert!(
            body.contains("ListenPort = 51820"),
            "нет ListenPort: {body}"
        );
        assert!(
            body.contains("PersistentKeepalive = 25"),
            "нет keepalive: {body}"
        );
    }

    #[test]
    fn ip_endpoint_parses_with_host_kept() {
        let parsed = parse_wg_config(&fixture("203.0.113.7:51821")).expect("валидный конфиг");
        assert_eq!(parsed.endpoint_host.as_deref(), Some("203.0.113.7"));
        assert_eq!(parsed.endpoint_port, 51821);
        assert!(!parsed.setconf_body.contains("Endpoint"));
    }

    #[test]
    fn address_mask_longer_than_32_is_stripped() {
        let text = fixture("vpn.example.com:51820").replace("10.8.0.2/32", "10.8.0.2/24");
        let parsed = parse_wg_config(&text).expect("конфиг с /24");
        assert_eq!(parsed.interface_addr, Ipv4Addr::new(10, 8, 0, 2));
    }

    #[test]
    fn garbage_config_is_rejected() {
        let err = parse_wg_config("это вообще не конфиг").expect_err("мусор должен быть отклонён");
        assert!(
            matches!(err, AgentError::InvalidConfig(_)),
            "ожидалась InvalidConfig: {err}"
        );
    }

    #[test]
    fn multiple_peer_sections_are_rejected_explicitly() {
        // До барьера второй [Peer] молча сливался в один setconf-кусок
        // и падал невнятной ошибкой wg setconf; теперь — явный отказ
        // на этапе разбора (ledger T10).
        let two_peers = format!(
            "[Interface]\n\
             Address = 10.8.0.2/32\n\
             PrivateKey = {PRIV}\n\
             \n\
             [Peer]\n\
             PublicKey = {PUB}\n\
             AllowedIPs = 10.0.0.0/8\n\
             Endpoint = vpn.example.com:51820\n\
             \n\
             [Peer]\n\
             PublicKey = FpCyhaw9L4Bk9CVnTuxEjpvGCnVPaKtIWQCBiV6LsSg=\n\
             AllowedIPs = 192.168.0.0/16\n\
             Endpoint = vpn2.example.com:51820\n"
        );
        let err = parse_wg_config(&two_peers).expect_err("два пира должны быть отклонены");
        assert!(
            matches!(err, AgentError::InvalidConfig(_)),
            "ожидалась InvalidConfig: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("один пир"),
            "ошибка обязана называть причину: {message}"
        );
    }

    #[test]
    fn missing_separator_error_does_not_leak_value() {
        // Строка без «=»: в ошибке только номер строки — значение
        // (здесь — секрет) не интерполируется.
        let err = parse_wg_config("[Interface]\nPrivateKey secret123abc\n")
            .expect_err("строка без «=» должна быть отклонена");
        let message = err.to_string();
        assert!(
            !message.contains("secret123abc"),
            "ошибка не должна раскрывать значение: {message}"
        );
        assert!(
            message.contains("строка 2"),
            "номер строки нужен для диагностики: {message}"
        );
    }

    #[test]
    fn key_outside_section_error_names_key_but_not_value() {
        // Ключ до любой секции: в ошибке имя ключа, но не значение.
        let err =
            parse_wg_config("PrivateKey = secret123abc\n[Interface]\nAddress = 10.8.0.2/32\n")
                .expect_err("ключ вне секции должен быть отклонён");
        let message = err.to_string();
        assert!(
            !message.contains("secret123abc"),
            "ошибка не должна раскрывать значение: {message}"
        );
        assert!(
            message.contains("PrivateKey"),
            "имя ключа безопасно и нужно для диагностики: {message}"
        );
    }

    #[test]
    fn unknown_section_error_shows_only_section_token() {
        let err =
            parse_wg_config("[Peers] PrivateKey = secret123abc\n").expect_err("неизвестная секция");
        let message = err.to_string();
        assert!(
            !message.contains("secret123abc"),
            "ошибка не должна раскрывать остаток строки: {message}"
        );
        assert!(
            message.contains("[Peers]"),
            "токен секции нужен для диагностики: {message}"
        );
    }

    #[test]
    fn wg_iface_name_is_per_slot() {
        assert_eq!(wg_iface_name(1), "wg1");
        assert_eq!(wg_iface_name(42), "wg42");
        assert_eq!(wg_iface_name(250), "wg250");
        assert_ne!(
            wg_iface_name(1),
            wg_iface_name(2),
            "слоты не должны делить имя"
        );
    }

    #[test]
    fn missing_private_key_is_rejected() {
        let text = fixture("vpn.example.com:51820").replace(&format!("PrivateKey = {PRIV}\n"), "");
        let err = parse_wg_config(&text).expect_err("нет PrivateKey");
        assert!(
            matches!(err, AgentError::InvalidConfig(_)),
            "ожидалась InvalidConfig: {err}"
        );
    }

    #[test]
    fn missing_peer_required_keys_are_rejected() {
        for key in ["PublicKey", "AllowedIPs", "Endpoint"] {
            let text = fixture("vpn.example.com:51820")
                .lines()
                .filter(|line| !line.starts_with(&format!("{key} =")))
                .collect::<Vec<_>>()
                .join("\n");
            let err = parse_wg_config(&text).expect_err(&format!("нет {key}"));
            assert!(matches!(err, AgentError::InvalidConfig(_)), "{key}: {err}");
        }
    }

    #[test]
    fn missing_interface_address_is_rejected() {
        let text = fixture("vpn.example.com:51820")
            .lines()
            .filter(|line| !line.starts_with("Address ="))
            .collect::<Vec<_>>()
            .join("\n");
        let err = parse_wg_config(&text).expect_err("нет Address");
        assert!(
            matches!(err, AgentError::InvalidConfig(_)),
            "ожидалась InvalidConfig: {err}"
        );
    }

    #[test]
    fn key_outside_section_is_rejected() {
        let err = parse_wg_config("PrivateKey = x\n").expect_err("ключ вне секции");
        assert!(
            matches!(err, AgentError::InvalidConfig(_)),
            "ожидалась InvalidConfig: {err}"
        );
    }

    #[test]
    fn unknown_wg_quick_keys_are_ignored() {
        let text = fixture("vpn.example.com:51820").replace(
            "ListenPort = 51820",
            "DNS = 1.1.1.1\nTable = off\nListenPort = 51820",
        );
        let parsed = parse_wg_config(&text).expect("посторонние ключи wg-quick не ломают разбор");
        assert!(
            !parsed.setconf_body.contains("DNS"),
            "DNS не для wg setconf: {}",
            parsed.setconf_body
        );
        assert!(
            !parsed.setconf_body.contains("Table"),
            "Table не для wg setconf: {}",
            parsed.setconf_body
        );
        assert!(parsed.setconf_body.contains("ListenPort = 51820"));
    }

    #[tokio::test]
    async fn resolve_endpoint_short_circuits_dotted_ipv4() {
        // IP-хост не требует обращения к системе — путь работает на любой ОС.
        let ip = resolve_endpoint_in_ns("какой-угодно-ns", "192.0.2.10")
            .await
            .expect("IP возвращается как есть");
        assert_eq!(ip, Ipv4Addr::new(192, 0, 2, 10));
    }
}
