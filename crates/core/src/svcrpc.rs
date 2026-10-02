//! RPC-протокол службы маршрутов (`bottle-svc`) и агента: управление
//! маршрутами ОС в сессиях бутылок.
//!
//! Сообщения передаются кадрами [`crate::frame`] (u32 BE длина + JSON)
//! по именованному каналу [`SVC_PIPE`]. Перечисления сериализуются
//! внешней пометкой serde по умолчанию (например, `{"Add":{...}}`).

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Имя канала службы маршрутов (Windows named pipe).
pub const SVC_PIPE: &str = r"\\.\pipe\bottleforapps-routes";

/// Запрос демона к службе маршрутов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SvcRequest {
    /// Рукопожатие: демон сообщает свой PID (служба привязывает
    /// маршруты к сессии соединения, а не к самому PID).
    Hello { daemon_pid: u32 },
    /// Добавить маршрут (dest → gateway через iface_alias) в сессию.
    Add {
        dest: IpAddr,
        gateway: IpAddr,
        iface_alias: String,
    },
    /// Удалить маршрут по назначению.
    Del { dest: IpAddr },
    /// Снести все живые маршруты службы (глобальная уборка, спека §9);
    /// на практике — маршруты единственного демона при его остановке.
    Cleanup,
    /// Список активных маршрутов службы.
    List,
}

/// Ответ службы маршрутов демону.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SvcReply {
    /// Успех без данных.
    Ok,
    /// Ответ на `List`: активные маршруты.
    RouteList { routes: Vec<RouteEntry> },
    /// Ошибка выполнения; `code` — константа [`svc_err`].
    Error { code: String, message: String },
}

/// Строка списка маршрутов службы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteEntry {
    /// Назначение маршрута.
    pub dest: IpAddr,
    /// Шлюз маршрута.
    pub gateway: IpAddr,
    /// Псевдоним интерфейса (индекс адаптера бутылки).
    pub iface_alias: String,
    /// Идентификатор сессии соединения демона: монотонный счётчик
    /// сессий службы (выделяется в `Hello`), а не PID демона.
    pub session: u64,
}

/// Ошибки валидации адреса назначения маршрута.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SvcError {
    /// Адрес из loopback-диапазонов: 127.0.0.0/8, ::1.
    #[error("адрес назначения loopback (127.0.0.0/8, ::1)")]
    Loopback,
    /// Адрес из link-local-диапазонов: 169.254.0.0/16, fe80::/10.
    #[error("адрес назначения link-local (169.254.0.0/16, fe80::/10)")]
    LinkLocal,
    /// Адрес из multicast-диапазонов: 224.0.0.0/4, ff00::/8.
    #[error("адрес назначения multicast (224.0.0.0/4, ff00::/8)")]
    Multicast,
}

/// Проверяет адрес назначения маршрута.
///
/// Отклоняет loopback (127.0.0.0/8, ::1), link-local (169.254.0.0/16,
/// fe80::/10) и multicast (224.0.0.0/4, ff00::/8); прочие адреса допустимы.
pub fn validate_route(dest: IpAddr) -> Result<(), SvcError> {
    match dest {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Err(SvcError::Loopback)
            } else if v4.is_link_local() {
                Err(SvcError::LinkLocal)
            } else if v4.is_multicast() {
                Err(SvcError::Multicast)
            } else {
                Ok(())
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Err(SvcError::Loopback)
            } else if v6.segments()[0] & 0xffc0 == 0xfe80 {
                // fe80::/10 — уникальные link-local адреса.
                Err(SvcError::LinkLocal)
            } else if v6.is_multicast() {
                Err(SvcError::Multicast)
            } else {
                Ok(())
            }
        }
    }
}

/// Ошибки валидации адреса шлюза маршрута.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    /// Шлюз unspecified: 0.0.0.0 / ::.
    #[error("шлюз unspecified (0.0.0.0, ::) — адрес не назначен ни одному интерфейсу")]
    Unspecified,
    /// Шлюз из loopback-диапазонов: 127.0.0.0/8, ::1.
    #[error("шлюз loopback (127.0.0.0/8, ::1) — не является достижимым next-hop")]
    Loopback,
    /// Шлюз — v4-broadcast: 255.255.255.255.
    #[error("широковещательный адрес 255.255.255.255 не может быть шлюзом")]
    Broadcast,
    /// Шлюз из link-local-диапазонов: 169.254.0.0/16, fe80::/10.
    #[error("шлюз link-local (169.254.0.0/16, fe80::/10) — без явного scope недостижим")]
    LinkLocal,
    /// Шлюз из multicast-диапазонов: 224.0.0.0/4, ff00::/8.
    #[error("шлюз multicast (224.0.0.0/4, ff00::/8) — next-hop обязан быть unicast")]
    Multicast,
}

/// Проверяет адрес шлюза маршрута (спека §4.2: привилегированная
/// поверхность службы обязана отвергать мусор до командлетов).
///
/// Шлюз — достижимый unicast-адрес: отвергаются unspecified
/// (0.0.0.0/::), loopback (127.0.0.0/8, ::1), v4-broadcast
/// (255.255.255.255) и link-local (169.254.0.0/16, fe80::/10 — без
/// zone id недостижимы); on-link ULA/приватные адреса допустимы
/// (`fd00::1`, `192.168.1.1`). Полная проверка «next-hop достижим
/// через конкретный интерфейс хоста» — M3 (нужен перечень адаптеров).
pub fn validate_gateway(gateway: IpAddr) -> Result<(), GatewayError> {
    match gateway {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() {
                Err(GatewayError::Unspecified)
            } else if v4.is_loopback() {
                Err(GatewayError::Loopback)
            } else if v4.is_broadcast() {
                Err(GatewayError::Broadcast)
            } else if v4.is_link_local() {
                Err(GatewayError::LinkLocal)
            } else if v4.is_multicast() {
                Err(GatewayError::Multicast)
            } else {
                Ok(())
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                Err(GatewayError::Unspecified)
            } else if v6.is_loopback() {
                Err(GatewayError::Loopback)
            } else if v6.segments()[0] & 0xffc0 == 0xfe80 {
                // fe80::/10 — link-local без zone id.
                Err(GatewayError::LinkLocal)
            } else if v6.is_multicast() {
                Err(GatewayError::Multicast)
            } else {
                Ok(())
            }
        }
    }
}

/// Коды ошибок службы маршрутов для wire-формата.
pub mod svc_err {
    /// Ошибка валидации параметров запроса.
    pub const VALIDATION: &str = "VALIDATION";
    /// Ошибка взаимодействия с ОС (маршруты, адаптеры).
    pub const IO: &str = "IO";
    /// Внутренняя ошибка службы.
    pub const INTERNAL: &str = "INTERNAL";
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

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
            .expect("encode must succeed for svcrpc types");
        codec
            .decode(&mut buf)
            .expect("decode must not fail on a well-formed frame")
            .expect("buffer must contain a complete frame")
    }

    fn sample_entry(session: u64) -> RouteEntry {
        RouteEntry {
            dest: IpAddr::from([10, 20, 30, 0]),
            gateway: IpAddr::from([10, 8, 0, 1]),
            iface_alias: "bottle-office-a".to_string(),
            session,
        }
    }

    #[test]
    fn svc_pipe_value_is_the_wire_contract() {
        assert_eq!(SVC_PIPE, r"\\.\pipe\bottleforapps-routes");
    }

    #[test]
    fn round_trip_every_request_variant() {
        let requests = vec![
            SvcRequest::Hello { daemon_pid: 4242 },
            SvcRequest::Add {
                dest: IpAddr::from([10, 20, 30, 0]),
                gateway: IpAddr::from([10, 8, 0, 1]),
                iface_alias: "bottle-office-a".to_string(),
            },
            SvcRequest::Add {
                dest: "2606:4700::".parse().expect("valid ipv6 dest"),
                gateway: "fd00::1".parse().expect("valid ipv6 gateway"),
                iface_alias: String::new(),
            },
            SvcRequest::Del {
                dest: IpAddr::from([10, 20, 30, 0]),
            },
            SvcRequest::Cleanup,
            SvcRequest::List,
        ];
        for req in requests {
            assert_eq!(round_trip(req.clone()), req);
        }
    }

    #[test]
    fn round_trip_every_reply_variant() {
        let replies = vec![
            SvcReply::Ok,
            SvcReply::RouteList {
                routes: vec![sample_entry(4242), sample_entry(9000)],
            },
            SvcReply::RouteList { routes: vec![] },
            SvcReply::Error {
                code: svc_err::VALIDATION.to_string(),
                message: "адрес назначения loopback".to_string(),
            },
            SvcReply::Error {
                code: svc_err::IO.to_string(),
                message: "CreateIpForwardEntry2 failed".to_string(),
            },
            SvcReply::Error {
                code: svc_err::INTERNAL.to_string(),
                message: String::new(),
            },
        ];
        for reply in replies {
            assert_eq!(round_trip(reply.clone()), reply);
        }
    }

    #[test]
    fn svc_err_codes_are_stable_wire_contract() {
        assert_eq!(svc_err::VALIDATION, "VALIDATION");
        assert_eq!(svc_err::IO, "IO");
        assert_eq!(svc_err::INTERNAL, "INTERNAL");
    }

    #[test]
    fn validate_route_accepts_ordinary_addresses() {
        let ok_addrs = ["8.8.8.8", "203.0.113.10", "2606:4700::1111", "10.8.0.1"];
        for addr in ok_addrs {
            let ip: IpAddr = addr.parse().expect("test address must parse");
            assert_eq!(validate_route(ip), Ok(()), "expected ok for {addr}");
        }
    }

    #[test]
    fn validate_route_rejects_special_ranges() {
        let cases = [
            ("127.0.0.1", SvcError::Loopback),
            ("127.1.2.3", SvcError::Loopback),
            ("::1", SvcError::Loopback),
            ("169.254.0.1", SvcError::LinkLocal),
            ("fe80::1", SvcError::LinkLocal),
            ("224.0.0.1", SvcError::Multicast),
            ("239.255.0.1", SvcError::Multicast),
            ("ff02::1", SvcError::Multicast),
        ];
        for (addr, expected) in cases {
            let ip: IpAddr = addr.parse().expect("test address must parse");
            assert_eq!(
                validate_route(ip),
                Err(expected),
                "expected reject for {addr}"
            );
        }
    }

    #[test]
    fn svc_error_display_is_russian_with_ranges() {
        assert_eq!(
            SvcError::Loopback.to_string(),
            "адрес назначения loopback (127.0.0.0/8, ::1)"
        );
        assert_eq!(
            SvcError::LinkLocal.to_string(),
            "адрес назначения link-local (169.254.0.0/16, fe80::/10)"
        );
        assert_eq!(
            SvcError::Multicast.to_string(),
            "адрес назначения multicast (224.0.0.0/4, ff00::/8)"
        );
    }

    /// Finding 3 (ревью): шлюз — достижимый unicast; приватные и
    /// ULA-адреса допустимы.
    #[test]
    fn validate_gateway_accepts_reachable_unicast() {
        let ok_gateways = [
            "192.168.1.1",
            "10.0.0.1",
            "8.8.8.8",
            "fd00::1",
            "2001:db8::1",
        ];
        for addr in ok_gateways {
            let ip: IpAddr = addr.parse().expect("test gateway must parse");
            assert_eq!(validate_gateway(ip), Ok(()), "expected ok for {addr}");
        }
    }

    /// Finding 3 (ревью): мусорные классы адресов отвергаются до
    /// командлетов (спека §4.2 — привилегированная поверхность).
    #[test]
    fn validate_gateway_rejects_unreachable_classes() {
        let cases = [
            ("0.0.0.0", GatewayError::Unspecified),
            ("::", GatewayError::Unspecified),
            ("127.0.0.1", GatewayError::Loopback),
            ("127.8.8.8", GatewayError::Loopback),
            ("::1", GatewayError::Loopback),
            ("255.255.255.255", GatewayError::Broadcast),
            ("169.254.0.1", GatewayError::LinkLocal),
            ("fe80::1", GatewayError::LinkLocal),
            ("224.0.0.1", GatewayError::Multicast),
            ("239.255.255.1", GatewayError::Multicast),
            ("ff02::1", GatewayError::Multicast),
        ];
        for (addr, expected) in cases {
            let ip: IpAddr = addr.parse().expect("test gateway must parse");
            assert_eq!(
                validate_gateway(ip),
                Err(expected),
                "expected reject for {addr}"
            );
        }
    }
}
