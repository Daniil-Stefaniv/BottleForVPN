//! RPC-протокол CLI и демона: команды пользователя и снимки состояния.
//!
//! Сообщения передаются кадрами [`crate::frame`] (u32 BE длина + JSON)
//! по именованному каналу [`DAEMON_PIPE`]. Перечисления сериализуются
//! внешней пометкой serde по умолчанию (например, `{"Start":{"id":"a"}}`).

use serde::{Deserialize, Serialize};

use crate::profile::{BottleKind, BottleProfile};
use crate::protocol::ForwardPort;

/// Имя канала демона (Windows named pipe).
pub const DAEMON_PIPE: &str = r"\\.\pipe\bottled";

/// Запрос CLI к демону.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliRequest {
    /// Список бутылок из хранилища (с известным состоянием).
    List,
    /// Показать профиль и/или рантайм-состояние бутылки.
    Show { id: String },
    /// Поднять бутылку.
    Start { id: String },
    /// Опустить бутылку.
    Stop { id: String },
    /// Рантайм-состояние всех поднятых бутылок.
    Status,
}

/// Ответ демона CLI.
///
/// `Show` крупнее остальных вариантов из-за встроенного профиля;
/// это редкое сообщение control-канала, поэтому размер оправдан
/// и боксирование не выполняется.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliReply {
    /// Ответ на `List`: бутылки хранилища.
    List { bottles: Vec<BottleSummary> },
    /// Ответ на `Show`: профиль с диска и/или рантайм-состояние;
    /// отсутствующая половина — `None`.
    Show {
        profile: Option<BottleProfile>,
        runtime: Option<RuntimeState>,
    },
    /// Бутылка поднята (ответ на `Start`).
    Started { id: String },
    /// Бутылка опущена (ответ на `Stop`).
    Stopped { id: String },
    /// Ответ на `Status`.
    Status { bottles: Vec<RuntimeState> },
    /// Ошибка выполнения команды.
    Error { message: String },
}

/// Строка списка бутылок: профиль с диска + известное состояние, если есть.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottleSummary {
    /// Идентификатор бутылки.
    pub id: String,
    /// Человекочитаемое имя.
    pub name: String,
    /// Тип бутылки (env/host).
    pub kind: BottleKind,
    /// Текущее состояние (`up`/...); `None` = не запущена/неизвестно.
    pub state: Option<String>,
}

/// Состояние бутылки в рантайме.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BottleState {
    /// Поднимается: туннель/SOCKS5/пробросы ещё настраиваются.
    Starting,
    /// Поднята и работает.
    Up,
    /// Работает с деградацией (например, часть пробросов недоступна).
    Degraded,
    /// Поднятие или работа завершились ошибкой.
    Failed { reason: String },
    /// Останавливается.
    Stopping,
    /// Остановлена.
    Stopped,
}

impl BottleState {
    /// Каноническое имя состояния для проводного формата и отображения.
    pub fn as_str(&self) -> &'static str {
        match self {
            BottleState::Starting => "starting",
            BottleState::Up => "up",
            BottleState::Degraded => "degraded",
            BottleState::Failed { .. } => "failed",
            BottleState::Stopping => "stopping",
            BottleState::Stopped => "stopped",
        }
    }
}

/// Рантайм-состояние одной бутылки (живые порты и состояние).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    /// Идентификатор бутылки.
    pub id: String,
    /// Состояние.
    pub state: BottleState,
    /// Назначенный порт SOCKS5 на 127.0.0.1; `None` = ещё не назначен.
    pub socks5: Option<u16>,
    /// Активные TCP-пробросы с назначенными портами слушателя.
    pub forwards: Vec<ForwardPort>,
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use crate::frame::LengthJsonCodec;
    use crate::profile::{BottleKind, BottleProfile, ExposeConfig, ForwardSpec, VpnConfig};
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
            .expect("encode must succeed for ipc types");
        codec
            .decode(&mut buf)
            .expect("decode must not fail on a well-formed frame")
            .expect("buffer must contain a complete frame")
    }

    fn sample_profile() -> BottleProfile {
        BottleProfile {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: crate::profile::TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
            },
            expose: ExposeConfig {
                socks5: 1080,
                forwards: vec![ForwardSpec {
                    listen: 33389,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some(IpAddr::from([10, 20, 30, 1])),
                launch: Some("mstsc".to_string()),
            },
        }
    }

    fn sample_runtime(id: &str, state: BottleState) -> RuntimeState {
        RuntimeState {
            id: id.to_string(),
            state,
            socks5: Some(1080),
            forwards: vec![crate::protocol::ForwardPort {
                port: 33389,
                target: "10.20.30.15:3389".to_string(),
            }],
        }
    }

    #[test]
    fn daemon_pipe_value_is_the_wire_contract() {
        assert_eq!(DAEMON_PIPE, r"\\.\pipe\bottled");
    }

    #[test]
    fn round_trip_every_request_variant() {
        let requests = vec![
            CliRequest::List,
            CliRequest::Show {
                id: "office-a".to_string(),
            },
            CliRequest::Start {
                id: "office-a".to_string(),
            },
            CliRequest::Stop {
                id: "office-a".to_string(),
            },
            CliRequest::Status,
        ];
        for req in requests {
            assert_eq!(round_trip(req.clone()), req);
        }
    }

    #[test]
    fn round_trip_every_reply_variant() {
        let summary = |state: Option<&str>| BottleSummary {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            state: state.map(str::to_string),
        };

        let replies = vec![
            CliReply::List {
                bottles: vec![
                    summary(Some("up")),
                    summary(None),
                    BottleSummary {
                        id: "corp-vpn".to_string(),
                        name: "Корп VPN".to_string(),
                        kind: BottleKind::Host,
                        state: Some("stopped".to_string()),
                    },
                ],
            },
            CliReply::List { bottles: vec![] },
            CliReply::Show {
                profile: Some(sample_profile()),
                runtime: Some(sample_runtime("office-a", BottleState::Up)),
            },
            CliReply::Show {
                profile: Some(sample_profile()),
                runtime: None,
            },
            CliReply::Show {
                profile: None,
                runtime: Some(sample_runtime(
                    "office-a",
                    BottleState::Failed {
                        reason: "туннель не поднялся".to_string(),
                    },
                )),
            },
            CliReply::Show {
                profile: None,
                runtime: None,
            },
            CliReply::Started {
                id: "office-a".to_string(),
            },
            CliReply::Stopped {
                id: "office-a".to_string(),
            },
            CliReply::Status {
                bottles: vec![
                    sample_runtime("office-a", BottleState::Up),
                    sample_runtime(
                        "corp-vpn",
                        BottleState::Failed {
                            reason: "туннель не поднялся".to_string(),
                        },
                    ),
                    RuntimeState {
                        id: "empty".to_string(),
                        state: BottleState::Stopped,
                        socks5: None,
                        forwards: vec![],
                    },
                ],
            },
            CliReply::Status { bottles: vec![] },
            CliReply::Error {
                message: "bottle not found".to_string(),
            },
        ];
        for reply in replies {
            assert_eq!(round_trip(reply.clone()), reply);
        }
    }

    #[test]
    fn bottle_state_as_str_matches_wire_contract() {
        assert_eq!(BottleState::Starting.as_str(), "starting");
        assert_eq!(BottleState::Up.as_str(), "up");
        assert_eq!(BottleState::Degraded.as_str(), "degraded");
        assert_eq!(
            BottleState::Failed {
                reason: "туннель не поднялся".to_string()
            }
            .as_str(),
            "failed"
        );
        assert_eq!(BottleState::Stopping.as_str(), "stopping");
        assert_eq!(BottleState::Stopped.as_str(), "stopped");
    }

    #[test]
    fn failed_state_round_trips_with_reason() {
        let state = BottleState::Failed {
            reason: "туннель не поднялся".to_string(),
        };
        let json = serde_json::to_string(&state).expect("serialize BottleState");
        assert_eq!(json, r#"{"Failed":{"reason":"туннель не поднялся"}}"#);
        assert_eq!(round_trip(state.clone()), state);
    }
}
