//! Модель профиля бутылки: разбор, валидация и канонический рендер TOML.
//!
//! Формат соответствует спеке §5.1: `[vpn]` — таблица с формой env
//! (`protocol` + `config`) или host (`client`/`args`/`endpoints`/`interface`),
//! выбираемой по набору полей; `[expose]` одинаков для обоих `kind`.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Тип бутылки: изолирующее окружение (WSL) или запуск на хосте.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BottleKind {
    Env,
    Host,
}

/// Протокол userspace-туннеля env-бутылки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelProtocol {
    WireGuard,
    OpenVpn,
}

/// Конфигурация VPN: файл-копия конфига в хранилище (env) или фирменный
/// клиент (host). Форму определяет набор полей `[vpn]`, поэтому serde
/// разбирает перечисление без тега (`untagged`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VpnConfig {
    /// `kind = "env"`: протокол и имя файла-копии конфига в хранилище.
    Env {
        protocol: TunnelProtocol,
        /// Относительное имя файла внутри хранилища бутылки.
        config: String,
    },
    /// `kind = "host"`: запуск фирменного VPN-клиента.
    Host {
        /// Путь к исполняемому файлу клиента.
        client: String,
        /// Аргументы запуска; опционально.
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        /// Адреса VPN-шлюзов, чей DNS перепроверяется.
        endpoints: Vec<String>,
        /// Имя сетевого интерфейса; опционально, иначе автоопределение.
        #[serde(skip_serializing_if = "Option::is_none")]
        interface: Option<String>,
    },
}

/// Один TCP-проброс: слушатель на 127.0.0.1 → цель `host:port`
/// (host — IP или DNS-имя внутри туннеля).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    /// Порт слушателя; 0 = автоподбор из диапазона 49152–65535.
    pub listen: u16,
    /// Цель вида `host:port`.
    pub target: String,
}

/// Конфигурация входа приложения в бутылку; одинакова для обоих `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposeConfig {
    /// Порт SOCKS5 на 127.0.0.1; 0 = автоподбор.
    pub socks5: u16,
    /// TCP-пробросы; `listen = 0` = автоподбор.
    pub forwards: Vec<ForwardSpec>,
    /// DNS внутри namespace (env) или документируется как неиспользуемый (host); опционально.
    pub dns: Option<IpAddr>,
    /// Команда, запускаемая после перехода в `up`; опционально.
    pub launch: Option<String>,
}

/// Профиль бутылки — валидированное содержимое `bottles/<id>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottleProfile {
    /// Идентификатор `[a-z0-9-]`, начинается с буквы или цифры; имя файла = id.
    pub id: String,
    /// Человекочитаемое имя.
    pub name: String,
    pub kind: BottleKind,
    pub vpn: VpnConfig,
    pub expose: ExposeConfig,
}

/// Ошибки разбора и валидации профиля.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// id не соответствует `[a-z0-9-]` или не начинается с буквы/цифры.
    #[error("invalid profile id: expected [a-z0-9-] starting with a letter or digit")]
    InvalidId,
    /// Порт вне диапазона 0..=65535 (кроме 0 = автоподбор).
    #[error("invalid port: expected 0..=65535")]
    InvalidPort,
    /// Цель проброса не вида `host:port`.
    #[error("invalid forward target: expected host:port")]
    InvalidTarget,
    /// Обязательное поле отсутствует или пусто.
    #[error("missing or empty required field")]
    MissingField,
    /// Форма `[vpn]` не соответствует `kind` (env ↔ protocol/config, host ↔ client/endpoints).
    #[error("vpn section shape does not match kind")]
    VpnKindMismatch,
    /// Ошибка сериализации профиля.
    #[error(transparent)]
    Render(#[from] toml::ser::Error),
    /// Ошибка разбора TOML.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

/// Проверяет id бутылки: непустой, только `[a-z0-9-]`, начинается с буквы или цифры.
pub fn validate_id(id: &str) -> Result<(), ProfileError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(ProfileError::InvalidId)
    }
}

/// Разбирает текст профиля (TOML) и валидирует его.
///
/// Ошибки структуры и неизвестных полей возвращаются как [`ProfileError::Toml`],
/// ошибки домена (id, порты, цели, форма `[vpn]`) — точными вариантами.
pub fn parse_profile(text: &str) -> Result<BottleProfile, ProfileError> {
    let raw: RawProfile = toml::from_str(text)?;

    validate_id(&raw.id)?;
    validate_vpn(raw.kind, &raw.vpn)?;

    let expose = raw.expose;
    let socks5 = u16::try_from(expose.socks5).map_err(|_| ProfileError::InvalidPort)?;
    let mut forwards = Vec::with_capacity(expose.forwards.len());
    for f in expose.forwards {
        let listen = u16::try_from(f.listen).map_err(|_| ProfileError::InvalidPort)?;
        validate_target(&f.target)?;
        forwards.push(ForwardSpec {
            listen,
            target: f.target,
        });
    }

    Ok(BottleProfile {
        id: raw.id,
        name: raw.name,
        kind: raw.kind,
        vpn: raw.vpn,
        expose: ExposeConfig {
            socks5,
            forwards,
            dns: expose.dns,
            launch: expose.launch,
        },
    })
}

/// Рендерит профиль в канонический TOML (обратный к [`parse_profile`]).
pub fn render_profile(p: &BottleProfile) -> Result<String, ProfileError> {
    Ok(toml::to_string(p)?)
}

/// Проверяет соответствие формы `[vpn]` значению `kind` и непустоту
/// обязательных полей.
fn validate_vpn(kind: BottleKind, vpn: &VpnConfig) -> Result<(), ProfileError> {
    match (kind, vpn) {
        (BottleKind::Env, VpnConfig::Env { config, .. }) => {
            if config.is_empty() {
                Err(ProfileError::MissingField)
            } else {
                Ok(())
            }
        }
        (
            BottleKind::Host,
            VpnConfig::Host {
                client, endpoints, ..
            },
        ) => {
            if client.is_empty() || endpoints.is_empty() || endpoints.iter().any(String::is_empty) {
                Err(ProfileError::MissingField)
            } else {
                Ok(())
            }
        }
        _ => Err(ProfileError::VpnKindMismatch),
    }
}

/// Проверяет цель проброса: `host:port`, host непустой, port 0..=65535.
/// IPv6-хост допускается в скобочной форме (`[::1]:3389`).
fn validate_target(target: &str) -> Result<(), ProfileError> {
    let Some((host, port)) = target.rsplit_once(':') else {
        return Err(ProfileError::InvalidTarget);
    };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(ProfileError::InvalidTarget);
    }
    Ok(())
}

// Служебные структуры разбора: порты читаются как u32, чтобы значение выше
// 65535 превращалось в InvalidPort, а не в ошибку TOML на этапе десериализации.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    id: String,
    name: String,
    kind: BottleKind,
    vpn: VpnConfig,
    #[serde(default)]
    expose: RawExposeConfig,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawExposeConfig {
    #[serde(default)]
    socks5: u32,
    #[serde(default)]
    forwards: Vec<RawForwardSpec>,
    #[serde(default)]
    dns: Option<IpAddr>,
    #[serde(default)]
    launch: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForwardSpec {
    listen: u32,
    target: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Спека §5.1: пример kind="env" дословно, без закомментированного host-блока.
    const SPEC_ENV_TOML: &str = r#"
id = "office-a"
name = "Офис А"
kind = "env"

[vpn]
protocol = "wireguard"
config = "wg-office-a.conf"

[expose]
socks5 = 0
forwards = [{ listen = 0, target = "10.20.30.15:3389" }]
dns = "10.20.30.1"
launch = "mstsc"
"#;

    // Спека §5.1: раскомментированный вид kind="host".
    const SPEC_HOST_TOML: &str = r#"
id = "corp-vpn"
name = "Корп VPN"
kind = "host"

[vpn]
client = "C:\\Program Files\\Vendor\\vpn.exe"
args = "connect office"
endpoints = ["vpn.example.com"]
interface = "Vendor Adapter"

[expose]
socks5 = 0
forwards = []
"#;

    /// Канонический текст = точный вывод `render_profile` для этого профиля.
    /// Замечание: непустые `forwards` рендерятся секцией `[[expose.forwards]]`
    /// (эквивалент инлайн-форме из спеки §5.1, которая тоже парсится).
    fn env_canonical_toml() -> String {
        [
            r#"id = "office-a""#,
            r#"name = "Офис А""#,
            r#"kind = "env""#,
            "",
            "[vpn]",
            r#"protocol = "wireguard""#,
            r#"config = "wg-office-a.conf""#,
            "",
            "[expose]",
            "socks5 = 0",
            r#"dns = "10.20.30.1""#,
            r#"launch = "mstsc""#,
            "",
            "[[expose.forwards]]",
            "listen = 0",
            r#"target = "10.20.30.15:3389""#,
        ]
        .join("\n")
            + "\n"
    }

    /// Путь с обратными слэшами рендерится literal-строкой в одинарных кавычках.
    fn host_canonical_toml() -> String {
        [
            r#"id = "corp-vpn""#,
            r#"name = "Корп VPN""#,
            r#"kind = "host""#,
            "",
            "[vpn]",
            r"client = 'C:\Program Files\Vendor\vpn.exe'",
            r#"args = "connect office""#,
            r#"endpoints = ["vpn.example.com"]"#,
            r#"interface = "Vendor Adapter""#,
            "",
            "[expose]",
            "socks5 = 0",
            "forwards = []",
        ]
        .join("\n")
            + "\n"
    }

    /// Минимальный валидный env-профиль; `expose` подставляется тестом.
    fn env_with_expose(expose: &str) -> String {
        format!(
            r#"
id = "a"
name = "A"
kind = "env"

[vpn]
protocol = "wireguard"
config = "c.conf"
{expose}
"#
        )
    }

    #[test]
    fn spec_env_example_parses() {
        let p = parse_profile(SPEC_ENV_TOML).expect("spec env profile must parse");

        assert_eq!(p.id, "office-a");
        assert_eq!(p.name, "Офис А");
        assert_eq!(p.kind, BottleKind::Env);
        assert_eq!(
            p.vpn,
            VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
            }
        );
        assert_eq!(p.expose.socks5, 0);
        assert_eq!(
            p.expose.forwards,
            vec![ForwardSpec {
                listen: 0,
                target: "10.20.30.15:3389".to_string(),
            }]
        );
        assert_eq!(p.expose.dns, Some("10.20.30.1".parse().unwrap()));
        assert_eq!(p.expose.launch.as_deref(), Some("mstsc"));
    }

    #[test]
    fn host_profile_parses() {
        let p = parse_profile(SPEC_HOST_TOML).expect("host profile must parse");

        assert_eq!(p.id, "corp-vpn");
        assert_eq!(p.kind, BottleKind::Host);
        assert_eq!(
            p.vpn,
            VpnConfig::Host {
                client: r"C:\Program Files\Vendor\vpn.exe".to_string(),
                args: Some("connect office".to_string()),
                endpoints: vec!["vpn.example.com".to_string()],
                interface: Some("Vendor Adapter".to_string()),
            }
        );
        assert_eq!(p.expose.socks5, 0);
        assert!(p.expose.forwards.is_empty());
    }

    #[test]
    fn validate_id_rules() {
        for id in ["office-a", "a", "bottle-2", "x0"] {
            validate_id(id).unwrap_or_else(|e| panic!("expected valid id {id:?}: {e}"));
        }
        for id in ["../evil", "Офис", "", "-x"] {
            assert!(
                matches!(validate_id(id), Err(ProfileError::InvalidId)),
                "expected InvalidId for {id:?}"
            );
        }
    }

    #[test]
    fn port_out_of_range_is_rejected() {
        let socks5 = env_with_expose("[expose]\nsocks5 = 65536");
        assert!(matches!(
            parse_profile(&socks5),
            Err(ProfileError::InvalidPort)
        ));

        let forwards = env_with_expose(
            "[expose]\nforwards = [{ listen = 65536, target = \"10.20.30.15:3389\" }]",
        );
        assert!(matches!(
            parse_profile(&forwards),
            Err(ProfileError::InvalidPort)
        ));
    }

    #[test]
    fn target_without_port_is_rejected() {
        let text =
            env_with_expose("[expose]\nforwards = [{ listen = 0, target = \"10.20.30.15\" }]");
        assert!(matches!(
            parse_profile(&text),
            Err(ProfileError::InvalidTarget)
        ));
    }

    #[test]
    fn missing_expose_section_defaults() {
        let text = r#"
id = "a"
name = "A"
kind = "env"

[vpn]
protocol = "openvpn"
config = "c.ovpn"
"#;
        let p = parse_profile(text).expect("expose is optional");
        assert_eq!(p.expose.socks5, 0);
        assert!(p.expose.forwards.is_empty());
        assert_eq!(p.expose.dns, None);
        assert_eq!(p.expose.launch, None);
    }

    #[test]
    fn env_round_trip_is_stable() {
        let t = env_canonical_toml();
        let rendered = render_profile(&parse_profile(&t).unwrap()).unwrap();
        assert_eq!(rendered, t);
        // Рендер идемпотентен: повторный проход не меняет текст.
        let again = render_profile(&parse_profile(&rendered).unwrap()).unwrap();
        assert_eq!(again, rendered);
    }

    #[test]
    fn host_round_trip_is_stable() {
        let t = host_canonical_toml();
        let rendered = render_profile(&parse_profile(&t).unwrap()).unwrap();
        assert_eq!(rendered, t);
    }

    #[test]
    fn vpn_shape_must_match_kind() {
        let env_with_host_vpn = r#"
id = "a"
name = "A"
kind = "env"

[vpn]
client = "vpn.exe"
endpoints = ["e.example"]
"#;
        assert!(matches!(
            parse_profile(env_with_host_vpn),
            Err(ProfileError::VpnKindMismatch)
        ));

        let host_with_env_vpn = r#"
id = "a"
name = "A"
kind = "host"

[vpn]
protocol = "wireguard"
config = "c.conf"
"#;
        assert!(matches!(
            parse_profile(host_with_env_vpn),
            Err(ProfileError::VpnKindMismatch)
        ));
    }

    #[test]
    fn empty_required_vpn_fields_are_rejected() {
        let empty_config = r#"
id = "a"
name = "A"
kind = "env"

[vpn]
protocol = "wireguard"
config = ""
"#;
        assert!(matches!(
            parse_profile(empty_config),
            Err(ProfileError::MissingField)
        ));

        let empty_client = r#"
id = "a"
name = "A"
kind = "host"

[vpn]
client = ""
endpoints = ["e.example"]
"#;
        assert!(matches!(
            parse_profile(empty_client),
            Err(ProfileError::MissingField)
        ));

        let empty_endpoints = r#"
id = "a"
name = "A"
kind = "host"

[vpn]
client = "vpn.exe"
endpoints = []
"#;
        assert!(matches!(
            parse_profile(empty_endpoints),
            Err(ProfileError::MissingField)
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let top_level = r#"
id = "a"
name = "A"
kind = "env"
extra = 1

[vpn]
protocol = "wireguard"
config = "c.conf"
"#;
        assert!(matches!(
            parse_profile(top_level),
            Err(ProfileError::Toml(_))
        ));

        let in_expose = env_with_expose("[expose]\nbogus = true");
        assert!(matches!(
            parse_profile(&in_expose),
            Err(ProfileError::Toml(_))
        ));
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        let text = r#"
id = "a"
name = "A"
kind = "env"

[vpn]
protocol = "pptp"
config = "c.conf"
"#;
        assert!(matches!(parse_profile(text), Err(ProfileError::Toml(_))));
    }
}
