//! RPC-протокол демона и агента: сообщения управления бутылками.
//!
//! Сообщения передаются кадрами [`crate::frame`] (u32 BE длина + JSON).
//! Перечисления сериализуются внешней пометкой serde по умолчанию
//! (например, `{"Down":{"id":"office-a","instance":7}}`).
//!
//! Версия 2 добавила instance-поколения: демон сопровождает каждый `Up`
//! и `Down` номером поколения бутылки, агент отвергает `Down` чужого
//! поколения ([`err::STALE_INSTANCE`]) — защита от TOCTOU «Down проигравшего
//! рестарта убивает живую бутылку победителя».

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

use crate::profile::TunnelProtocol;

/// Версия RPC-протокола; сверяется в `Hello`/`HelloAck`.
pub const PROTOCOL_VERSION: u32 = 2;

/// Запрос демона к агенту.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRequest {
    /// Первое сообщение соединения: версия протокола и токен авторизации.
    Hello { version: u32, token: String },
    /// Поднять бутылку (туннель + SOCKS5 + пробросы).
    Up(UpArgs),
    /// Опустить бутылку своего поколения; чужое [`UpArgs::instance`]
    /// отвергается кодом [`err::STALE_INSTANCE`], бутылка не трогается.
    Down { id: String, instance: u64 },
    /// Статус одной бутылки (`Some`) или всех (`None`).
    Status { id: Option<String> },
    /// Завершить работу агента.
    Shutdown,
}

/// Аргументы запроса `Up`: что и как поднять внутри аплаенса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpArgs {
    /// Идентификатор бутылки.
    pub id: String,
    /// Протокол userspace-туннеля.
    pub protocol: TunnelProtocol,
    /// Содержимое файла конфигурации туннеля.
    pub config: String,
    /// DNS внутри namespace; отсутствует = не менять.
    pub dns: Option<IpAddr>,
    /// Порт SOCKS5 на 127.0.0.1; 0 = автоподбор.
    pub socks5_port: u16,
    /// TCP-пробросы на 127.0.0.1.
    pub forwards: Vec<ForwardPort>,
    /// Поколение экземпляра демона, поднимающего бутылку. Агент хранит
    /// его вместе с бутылкой и опускает её только `Down` с тем же
    /// номером — `Down` перезапущенного/проигравшего демона получает
    /// [`err::STALE_INSTANCE`] и не убивает живую бутылку.
    pub instance: u64,
}

/// Один TCP-проброс: слушатель на 127.0.0.1 → цель `host:port` в туннеле.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardPort {
    /// Порт слушателя.
    pub port: u16,
    /// Цель вида `host:port`.
    pub target: String,
}

/// Ответ агента демону.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentReply {
    /// Ответ на `Hello`: версии сторон и IP аплаенса в сети WSL NAT.
    HelloAck {
        version: u32,
        appliance_version: String,
        vm_ip: Ipv4Addr,
    },
    /// Бутылка поднята (ответ на `Up`).
    Ready { id: String },
    /// Ответ на `Status`.
    Status { bottles: Vec<BottleStatusInfo> },
    /// Успех без данных (ответ на `Down`/`Shutdown`).
    Ok,
    /// Ошибка: машинный код из [`err`] и человекочитаемое сообщение.
    Error { code: String, message: String },
}

/// Состояние одной бутылки в ответе `Status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottleStatusInfo {
    /// Идентификатор бутылки.
    pub id: String,
    /// Состояние (`up`/`down`/...).
    pub state: String,
    /// Детали состояния; отсутствует = без подробностей.
    pub detail: Option<String>,
}

/// Коды ошибок агента (поле `code` ответа `Error`).
pub mod err {
    /// Бутылка уже поднята.
    pub const ALREADY_UP: &str = "ALREADY_UP";
    /// Бутылка не найдена.
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// Не удалось поднять или настроить туннель.
    pub const TUNNEL_FAILED: &str = "TUNNEL_FAILED";
    /// Некорректный запрос.
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    /// Локальный порт занят.
    pub const PORT_BUSY: &str = "PORT_BUSY";
    /// Ошибка авторизации (токен).
    pub const AUTH: &str = "AUTH";
    /// Несовместимая версия протокола.
    pub const VERSION: &str = "VERSION";
    /// `Down` пришёл с чужим instance-поколением: живая бутылка
    /// принадлежит другому поколению демона и не трогается.
    pub const STALE_INSTANCE: &str = "STALE_INSTANCE";
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::frame::LengthJsonCodec;
    use bytes::BytesMut;
    use serde::de::DeserializeOwned;
    use tokio_util::codec::{Decoder, Encoder};

    /// Кодирует и декодирует значение через `LengthJsonCodec` (стиль `Framed`).
    fn round_trip<T>(value: T) -> T
    where
        T: Serialize + Clone + DeserializeOwned,
    {
        let mut codec = LengthJsonCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(value.clone(), &mut buf)
            .expect("encode must succeed for protocol types");
        codec
            .decode(&mut buf)
            .expect("decode must not fail on a well-formed frame")
            .expect("buffer must contain a complete frame")
    }

    #[test]
    fn round_trip_every_request_variant() {
        let requests = vec![
            AgentRequest::Hello {
                version: PROTOCOL_VERSION,
                token: "secret-token".to_string(),
            },
            AgentRequest::Up(UpArgs {
                id: "office-a".to_string(),
                protocol: TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
                dns: Some(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 1))),
                socks5_port: 1080,
                forwards: vec![ForwardPort {
                    port: 33389,
                    target: "10.20.30.15:3389".to_string(),
                }],
                instance: 7,
            }),
            AgentRequest::Up(UpArgs {
                id: "corp".to_string(),
                protocol: TunnelProtocol::OpenVpn,
                config: "corp.ovpn".to_string(),
                dns: None,
                socks5_port: 0,
                forwards: vec![],
                instance: 0,
            }),
            AgentRequest::Down {
                id: "office-a".to_string(),
                instance: 7,
            },
            AgentRequest::Down {
                id: "office-a".to_string(),
                instance: 8,
            },
            AgentRequest::Status {
                id: Some("office-a".to_string()),
            },
            AgentRequest::Status { id: None },
            AgentRequest::Shutdown,
        ];
        for req in requests {
            assert_eq!(round_trip(req.clone()), req);
        }
    }

    #[test]
    fn round_trip_every_reply_variant() {
        let replies = vec![
            AgentReply::HelloAck {
                version: PROTOCOL_VERSION,
                appliance_version: "0.1.0".to_string(),
                vm_ip: Ipv4Addr::new(192, 168, 77, 5),
            },
            AgentReply::Ready {
                id: "office-a".to_string(),
            },
            AgentReply::Status {
                bottles: vec![
                    BottleStatusInfo {
                        id: "office-a".to_string(),
                        state: "up".to_string(),
                        detail: Some("tunnel ready".to_string()),
                    },
                    BottleStatusInfo {
                        id: "corp".to_string(),
                        state: "down".to_string(),
                        detail: None,
                    },
                ],
            },
            AgentReply::Status { bottles: vec![] },
            AgentReply::Ok,
            AgentReply::Error {
                code: err::TUNNEL_FAILED.to_string(),
                message: "wireguard-go exited".to_string(),
            },
            AgentReply::Error {
                code: err::STALE_INSTANCE.to_string(),
                message: "бутылка принадлежит другому поколению демона".to_string(),
            },
        ];
        for reply in replies {
            assert_eq!(round_trip(reply.clone()), reply);
        }
    }

    #[test]
    fn up_args_json_contains_protocol_name() {
        let wireguard = UpArgs {
            id: "office-a".to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: "wg-office-a.conf".to_string(),
            dns: None,
            socks5_port: 1080,
            forwards: vec![],
            instance: 7,
        };
        let json = serde_json::to_string(&wireguard).expect("serialize UpArgs");
        assert!(
            json.contains(r#""protocol":"wireguard""#),
            "unexpected JSON: {json}"
        );
        assert!(
            json.contains(r#""instance":7"#),
            "instance-поколение присутствует на проводе: {json}"
        );

        let openvpn = UpArgs {
            protocol: TunnelProtocol::OpenVpn,
            ..wireguard
        };
        let json = serde_json::to_string(&openvpn).expect("serialize UpArgs");
        assert!(
            json.contains(r#""protocol":"openvpn""#),
            "unexpected JSON: {json}"
        );
    }

    #[test]
    fn error_code_constants_match_wire_contract() {
        assert_eq!(err::ALREADY_UP, "ALREADY_UP");
        assert_eq!(err::NOT_FOUND, "NOT_FOUND");
        assert_eq!(err::TUNNEL_FAILED, "TUNNEL_FAILED");
        assert_eq!(err::BAD_REQUEST, "BAD_REQUEST");
        assert_eq!(err::PORT_BUSY, "PORT_BUSY");
        assert_eq!(err::AUTH, "AUTH");
        assert_eq!(err::VERSION, "VERSION");
        assert_eq!(err::STALE_INSTANCE, "STALE_INSTANCE");
    }

    #[test]
    fn protocol_version_is_two() {
        // Версия 2 добавила instance-поколения в Up/Down; аплаенсы v1
        // отвергаются handshake-ошибкой VERSION (без обратной совместимости).
        assert_eq!(PROTOCOL_VERSION, 2);
    }
}
