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
    /// Идентификатор `[a-z0-9-]`, начинается с буквы или цифры, не
    /// совпадает с резервированным именем Windows; имя файла = id.
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
    /// id не соответствует `[a-z0-9-]`, не начинается с буквы/цифры
    /// или совпадает с резервированным именем устройства Windows.
    #[error("invalid profile id: expected [a-z0-9-] starting with a letter or digit, not a Windows reserved name")]
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

/// Есть ли в конфиге OpenVPN голая директива `auth-user-pass` (без
/// файла-аргумента): такой конфиг требует логин/пароль при каждом
/// запуске — openvpn читает их из auth-файла, который подставляет агент.
///
/// Используется и демоном (запрос кредов до обращения к аплаенсу),
/// и агентом (переписывание директивы на auth-файл).
pub fn openvpn_config_needs_auth(config: &str) -> bool {
    config.lines().any(is_bare_auth_user_pass_line)
}

/// Строка — голый `auth-user-pass`: первый токен совпадает, аргументов
/// нет (комментарий `#` аргументом не считается).
pub fn is_bare_auth_user_pass_line(line: &str) -> bool {
    let line = line.trim();
    let mut tokens = line.split_whitespace();
    if tokens.next() != Some("auth-user-pass") {
        return false;
    }
    match tokens.next() {
        None => true,
        Some(arg) => arg.starts_with('#'),
    }
}

/// Резервированные имена устройств Windows. id бутылки становится именем
/// файла `bottles/<id>.toml`, а Windows считает такие имена устройствами
/// независимо от расширения (`nul.toml` так же неоткрываем, как `nul`) —
/// поэтому точное совпадение с ними запрещено. Алфавит id уже ограничен
/// lowercase (`[a-z0-9-]`), так что регистронезависимость покрыта самим
/// алфавитом и достаточно прямого сравнения.
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", //
    "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", //
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Проверяет id бутылки: непустой, только `[a-z0-9-]`, начинается с
/// буквы или цифры и не совпадает с резервированным именем устройства
/// Windows (см. [`WINDOWS_RESERVED_NAMES`]).
pub fn validate_id(id: &str) -> Result<(), ProfileError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !WINDOWS_RESERVED_NAMES.contains(&id);
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

/// Валидирует готовый профиль (создание через `bottle create`): тот же
/// набор проверок, что у [`parse_profile`] — id, форма `[vpn]` по kind,
/// обязательные поля (env — config; host — client и endpoints), цели
/// пробросов. Порты — уже `u16` по построению структуры.
pub fn validate_profile(p: &BottleProfile) -> Result<(), ProfileError> {
    validate_id(&p.id)?;
    validate_vpn(p.kind, &p.vpn)?;
    for forward in &p.expose.forwards {
        validate_target(&forward.target)?;
    }
    Ok(())
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

/// Проверяет цель проброса: `host:port`, host непустой, port 1..=65535
/// (0 — автоподбор только локальных портов; цель с нулевым портом
/// недостижима — релей отвергает её при разборе `TargetAddr`, профиль
/// ловит то же на валидации). IPv6-хост допускается в скобочной форме
/// (`[::1]:3389`).
fn validate_target(target: &str) -> Result<(), ProfileError> {
    let Some((host, port)) = target.rsplit_once(':') else {
        return Err(ProfileError::InvalidTarget);
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| ProfileError::InvalidTarget)?;
    if host.is_empty() || port == 0 {
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
    fn windows_reserved_ids_are_rejected() {
        // id = имя файла bottles/<id>.toml: точное совпадение с
        // резервированным именем Windows неоткрываемо/опасно.
        for id in [
            "con", "prn", "aux", "nul", "com1", "com5", "com9", "lpt1", "lpt9",
        ] {
            assert!(
                matches!(validate_id(id), Err(ProfileError::InvalidId)),
                "expected InvalidId for reserved id {id:?}"
            );
        }
        // Режется только точное совпадение: похожие id легальны.
        for id in ["console", "contact", "aux2x", "com1x", "nul0"] {
            validate_id(id).unwrap_or_else(|e| panic!("expected valid id {id:?}: {e}"));
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

    /// Порт 0 — автоподбор только для локальных портов (socks5/listen);
    /// цель проброса с нулевым портом недостижима. Релей отвергает её
    /// при разборе TargetAddr (1..=65535) — профиль должен ловить это
    /// раньше, на валидации (T13: синхрон с relay-lib).
    #[test]
    fn target_with_port_zero_is_rejected() {
        let text =
            env_with_expose("[expose]\nforwards = [{ listen = 0, target = \"10.20.30.15:0\" }]");
        assert!(
            matches!(parse_profile(&text), Err(ProfileError::InvalidTarget)),
            "порт 0 в цели проброса недостижим — InvalidTarget, не молчаливый приём"
        );
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

    // ---------------------------------------------------------------
    // validate_profile (создание через `bottle create`).
    // ---------------------------------------------------------------

    fn valid_env_profile() -> BottleProfile {
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
                forwards: vec![],
                dns: None,
                launch: None,
            },
        }
    }

    #[test]
    fn validate_profile_accepts_valid_env_and_host() {
        let mut env = valid_env_profile();
        assert!(validate_profile(&env).is_ok());

        env.kind = BottleKind::Host;
        env.vpn = VpnConfig::Host {
            client: r"C:\vpn.exe".to_string(),
            args: None,
            endpoints: vec!["vpn.example.com".to_string()],
            interface: None,
        };
        assert!(validate_profile(&env).is_ok());
    }

    /// Обязательные поля host (Review Focus M3 #5): без endpoint'ов
    /// или без клиента создание невозможно.
    #[test]
    fn validate_profile_rejects_host_without_endpoints_or_client() {
        let mut p = valid_env_profile();
        p.kind = BottleKind::Host;
        p.vpn = VpnConfig::Host {
            client: r"C:\vpn.exe".to_string(),
            args: None,
            endpoints: vec![],
            interface: None,
        };
        assert!(matches!(
            validate_profile(&p),
            Err(ProfileError::MissingField)
        ));

        p.vpn = VpnConfig::Host {
            client: String::new(),
            args: None,
            endpoints: vec!["vpn.example.com".to_string()],
            interface: None,
        };
        assert!(matches!(
            validate_profile(&p),
            Err(ProfileError::MissingField)
        ));
    }

    /// env без конфига (пустой путь) невозможен.
    #[test]
    fn validate_profile_rejects_env_without_config() {
        let mut p = valid_env_profile();
        p.vpn = VpnConfig::Env {
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
        };
        assert!(matches!(
            validate_profile(&p),
            Err(ProfileError::MissingField)
        ));
    }

    #[test]
    fn validate_profile_rejects_bad_id_and_target() {
        let mut p = valid_env_profile();
        p.id = "../evil".to_string();
        assert!(matches!(validate_profile(&p), Err(ProfileError::InvalidId)));

        let mut p = valid_env_profile();
        p.expose.forwards = vec![ForwardSpec {
            listen: 0,
            target: "no-port".to_string(),
        }];
        assert!(matches!(
            validate_profile(&p),
            Err(ProfileError::InvalidTarget)
        ));
    }

    // ---------------------------------------------------------------
    // openvpn_config_needs_auth: детекция голого auth-user-pass.
    // ---------------------------------------------------------------

    #[test]
    fn openvpn_needs_auth_detects_bare_directive() {
        assert!(openvpn_config_needs_auth("client\nauth-user-pass\n"));
        assert!(openvpn_config_needs_auth("  auth-user-pass  \n"));
        // Комментарий после директивы — всё ещё голая форма.
        assert!(openvpn_config_needs_auth("auth-user-pass # спросить\n"));
    }

    #[test]
    fn openvpn_needs_auth_ignores_file_form_and_other_directives() {
        assert!(!openvpn_config_needs_auth("auth-user-pass /etc/auth.txt\n"));
        assert!(!openvpn_config_needs_auth("auth-user-pass auth.txt\n"));
        assert!(!openvpn_config_needs_auth("# auth-user-pass\n"));
        assert!(!openvpn_config_needs_auth(
            "client\nremote vpn.example 443\n"
        ));
        assert!(!openvpn_config_needs_auth(""));
    }
}
