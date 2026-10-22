//! Диагностика `bottle doctor`: проверки окружения с итогами
//! ok/warn/fail и русскими пояснениями (спека §4.4, §15). Проверки —
//! чистые функции над собранными данными; сборщик [`run`] обращается
//! к WSL/менеджеру/хранилищу. Принцип (Review Focus M3 #4): чистая
//! машина не даёт ложно-красного — нормальные для portable/холодного
//! старта состояния — `warn`, `fail` только когда без действий
//! пользователя функциональность недоступна.

use std::io;
use std::net::TcpListener;

use bottle_core::ipc::{CheckItem, CheckStatus};
use bottle_core::profile::BottleProfile;

use crate::appliance::{WslRunner, DISTRIBUTION};
use crate::error::DaemonError;
use crate::manager::BottleManager;

/// Итог bind-пробы фиксированного порта: (порт, свободен, владелец-
/// бутылка если порт занят живым слушателем демона).
pub struct PortProbe {
    /// Проверенный порт.
    pub port: u16,
    /// Удалось ли занять порт (свободен).
    pub free: bool,
    /// Бутылка-владелец, если порт занят живым слушателем демона.
    pub live_bottle: Option<String>,
}

/// Собирает отчёт doctor: WSL (`--status`, `-l -q`, `.wslconfig`),
/// аплаенс, служба маршрутов, агент, профили, фиксированные порты.
pub async fn run(
    wsl: &dyn WslRunner,
    registry: &crate::registry::SharedRegistry,
    manager: &BottleManager,
) -> Vec<CheckItem> {
    let wsl_status = wsl.run(&["--status"], None).await;
    let wsl_list = wsl.run(&["-l", "-q"], None).await;
    let wsl_version = wsl.run(&["--version"], None).await;
    let wslconfig = std::env::var_os("USERPROFILE")
        .map(|home| std::path::Path::new(&home).join(".wslconfig"))
        .and_then(|path| std::fs::read_to_string(path).ok());

    let installed = matches!(wsl_list.as_deref(), Ok(listing) if is_installed(listing));
    let appliance_version = manager.appliance_version().await;
    let profiles: Vec<(String, Result<BottleProfile, DaemonError>)> = {
        let registry = registry.read().expect("registry poisoned");
        registry
            .ids()
            .iter()
            .filter_map(|id| {
                registry
                    .get(id)
                    .map(|full| (id.clone(), Ok(full.profile)))
            })
            .collect()
    };
    let live_ports = live_listener_ports(manager).await;

    let mut items = check_wsl(&wsl_status, &wsl_list, &wslconfig);
    items.push(check_wsl_version(&wsl_version));
    items.push(check_appliance(installed, &appliance_version));
    items.push(check_service(crate::svc_client::svc_pipe_exists()));
    items.push(check_agent(installed, &appliance_version));
    items.push(check_profiles(&profiles));

    let probes = probe_fixed_ports(&profiles, &live_ports).await;
    items.extend(check_ports(&probes));
    items
}

/// Порты живых слушателей демона: (id бутылки, порты).
async fn live_listener_ports(manager: &BottleManager) -> Vec<(String, Vec<u16>)> {
    manager
        .status()
        .await
        .into_iter()
        .map(|state| {
            let mut ports = Vec::new();
            if let Some(port) = state.socks5 {
                ports.push(port);
            }
            ports.extend(state.forwards.iter().map(|forward| forward.port));
            (state.id, ports)
        })
        .collect()
}

/// Пробует занять фиксированные порты профилей на 127.0.0.1 (bind и
/// сразу освободить); порт живой бутылки демона считается занятым
/// легально.
async fn probe_fixed_ports(
    profiles: &[(String, Result<BottleProfile, DaemonError>)],
    live_ports: &[(String, Vec<u16>)],
) -> Vec<PortProbe> {
    let mut probes = Vec::new();
    for (_, result) in profiles {
        let Ok(profile) = result else {
            continue;
        };
        for port in fixed_ports(profile) {
            let free = TcpListener::bind(("127.0.0.1", port)).is_ok();
            let live_bottle = live_ports
                .iter()
                .find(|(_, ports)| ports.contains(&port))
                .map(|(id, _)| id.clone());
            probes.push(PortProbe {
                port,
                free,
                live_bottle,
            });
        }
    }
    probes
}

/// Фиксированные порты профиля (0 = автоподбор, не проверяется).
fn fixed_ports(profile: &BottleProfile) -> Vec<u16> {
    let mut ports = Vec::new();
    if profile.expose.socks5 != 0 {
        ports.push(profile.expose.socks5);
    }
    ports.extend(
        profile
            .expose
            .forwards
            .iter()
            .map(|forward| forward.listen)
            .filter(|port| *port != 0),
    );
    ports
}

/// Есть ли дистрибутив аплаенса в выводе `wsl -l -q` (чистка BOM/\r;
/// паттерн `appliance::is_installed`, недоступного отсюда).
fn is_installed(listing: &str) -> bool {
    listing.lines().any(|line| {
        line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}') == DISTRIBUTION
    })
}

/// Проверка WSL: выключен → fail с командой включения; включён без
/// дистрибуций → warn (норма для portable — аплаенс импортируется при
/// первом старте); включён с дистрибуциями → ok. Отдельной строкой —
/// режим сети из `.wslconfig`: mirrored → fail (схема требует NAT,
/// спека §6).
pub fn check_wsl(
    status: &io::Result<String>,
    list: &io::Result<String>,
    wslconfig: &Option<String>,
) -> Vec<CheckItem> {
    let mut items = Vec::new();
    let wsl_item = match (status, list) {
        (Err(error), _) => CheckItem {
            code: "wsl".to_string(),
            status: CheckStatus::Fail,
            detail: format!(
                "WSL недоступен ({error}) — включите: wsl --install --no-distribution, \
                 затем перезагрузите компьютер"
            ),
        },
        (Ok(_), Err(error)) => CheckItem {
            code: "wsl".to_string(),
            status: CheckStatus::Warn,
            detail: format!("WSL включён, список дистрибуций не получен: {error}"),
        },
        (Ok(_), Ok(listing)) => {
            let count = listing
                .lines()
                .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}'))
                .filter(|line| !line.is_empty())
                .count();
            if count == 0 {
                CheckItem {
                    code: "wsl".to_string(),
                    status: CheckStatus::Warn,
                    detail: "WSL включён, дистрибуций нет — нормально до первого старта \
                             бутылки (аплаенс импортируется автоматически)"
                        .to_string(),
                }
            } else {
                CheckItem {
                    code: "wsl".to_string(),
                    status: CheckStatus::Ok,
                    detail: format!("WSL включён, дистрибуций: {count}"),
                }
            }
        }
    };
    items.push(wsl_item);

    if let Some(text) = wslconfig {
        if text.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase();
            line.starts_with("networkingmode") && line.ends_with("mirrored")
        }) {
            items.push(CheckItem {
                code: "wsl-networking".to_string(),
                status: CheckStatus::Fail,
                detail: "Режим сети WSL — mirrored; требуется NAT: уберите \
                         networkingMode=mirrored из .wslconfig и выполните wsl --shutdown"
                    .to_string(),
            });
        }
    }
    items
}

/// Версия WSL: современный (Store) wsl.exe отвечает на `--version`
/// (ok, первая строка с номером); inbox wsl.exe такой опции не знает —
/// warn с пояснением, что вывод декодируется с учётом UTF-16LE
/// (appliance::decode_wsl_output). Не fail: inbox WSL работает.
pub fn check_wsl_version(version: &io::Result<String>) -> CheckItem {
    match version {
        Ok(text) => {
            let first = text.lines().next().unwrap_or_default().trim();
            CheckItem {
                code: "wsl-version".to_string(),
                status: CheckStatus::Ok,
                detail: format!("Версия WSL: {first}"),
            }
        }
        Err(_) => CheckItem {
            code: "wsl-version".to_string(),
            status: CheckStatus::Warn,
            detail: "wsl --version не поддерживается — старый inbox wsl.exe; \
                     его вывод (UTF-16LE) декодируется автоматически"
                .to_string(),
        },
    }
}
/// последнего handshake (если сессия уже подключалась).
pub fn check_appliance(installed: bool, version: &Option<String>) -> CheckItem {
    match (installed, version) {
        (false, _) => CheckItem {
            code: "appliance".to_string(),
            status: CheckStatus::Warn,
            detail: "Аплаенс не импортирован — импортируется при первом старте бутылки".to_string(),
        },
        (true, Some(version)) => CheckItem {
            code: "appliance".to_string(),
            status: CheckStatus::Ok,
            detail: format!("Аплаенс импортирован, версия {version}"),
        },
        (true, None) => CheckItem {
            code: "appliance".to_string(),
            status: CheckStatus::Ok,
            detail: "Аплаенс импортирован (версия уточнится при первом старте)".to_string(),
        },
    }
}

/// Проверка службы маршрутов: канал есть → ok (host-бутылки
/// доступны); нет → warn env-only с подсказкой установки.
pub fn check_service(present: bool) -> CheckItem {
    if present {
        CheckItem {
            code: "service".to_string(),
            status: CheckStatus::Ok,
            detail: "Служба маршрутов установлена — host-бутылки доступны".to_string(),
        }
    } else {
        CheckItem {
            code: "service".to_string(),
            status: CheckStatus::Warn,
            detail: "Служба маршрутов не установлена — только env-бутылки; \
                     для host-бутылок выполните от администратора: bottle-svc --install"
                .to_string(),
        }
    }
}

/// Проверка агента аплаенса — пассивная (finding 2 финального
/// ревью M3): doctor не будит WSL и не импортирует аплаенс.
/// `installed && connected` (версия известна из handshake текущей
/// сессии) → ok; импортирован, но сессия не подключалась → warn
/// (проверка отложена к первому старту); не импортирован → warn.
/// Несовместимая версия проявляется отказом handshake при старте —
/// с понятной подсказкой `bottle upgrade` (спека §8).
pub fn check_agent(installed: bool, version: &Option<String>) -> CheckItem {
    match (installed, version) {
        (true, Some(version)) => CheckItem {
            code: "agent".to_string(),
            status: CheckStatus::Ok,
            detail: format!("Агент аплаенса отвечает (версия {version})"),
        },
        (true, None) => CheckItem {
            code: "agent".to_string(),
            status: CheckStatus::Warn,
            detail: "Аплаенс импортирован; агент проверится при первом старте бутылки \
                     (doctor не будит WSL)"
                .to_string(),
        },
        (false, _) => CheckItem {
            code: "agent".to_string(),
            status: CheckStatus::Warn,
            detail: "Агент аплаенса недоступен — аплаенс не импортирован (нормально \
                     до первого старта)"
                .to_string(),
        },
    }
}

/// Проверка профилей: все читаются → ok с количеством; битые → warn
/// с перечнем id (бутылка не поднимется, пока профиль не поправят).
pub fn check_profiles(profiles: &[(String, Result<BottleProfile, DaemonError>)]) -> CheckItem {
    let broken: Vec<&str> = profiles
        .iter()
        .filter(|(_, result)| result.is_err())
        .map(|(id, _)| id.as_str())
        .collect();
    if broken.is_empty() {
        CheckItem {
            code: "profiles".to_string(),
            status: CheckStatus::Ok,
            detail: format!("Профили читаются: {}", profiles.len()),
        }
    } else {
        CheckItem {
            code: "profiles".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "Битые профили (не поднимутся): {} — исправьте или удалите файлы в каталоге bottles",
                broken.join(", ")
            ),
        }
    }
}

/// Проверка фиксированных портов: свободны → ok одной строкой;
/// занят чужим процессом → warn (start этой бутылки откажет);
/// занят живой бутылкой демона → ok (легитимный слушатель).
pub fn check_ports(probes: &[PortProbe]) -> Vec<CheckItem> {
    if probes.is_empty() {
        return vec![CheckItem {
            code: "ports".to_string(),
            status: CheckStatus::Ok,
            detail: "Фиксированных портов в профилях нет".to_string(),
        }];
    }
    let busy: Vec<&PortProbe> = probes.iter().filter(|probe| !probe.free).collect();
    if busy.is_empty() {
        return vec![CheckItem {
            code: "ports".to_string(),
            status: CheckStatus::Ok,
            detail: format!("Фиксированные порты свободны: {}", probes.len()),
        }];
    }
    busy.into_iter()
        .map(|probe| match &probe.live_bottle {
            Some(id) => CheckItem {
                code: "ports".to_string(),
                status: CheckStatus::Ok,
                detail: format!(
                    "Порт {} занят живой бутылкой {id} — легитимный слушатель",
                    probe.port
                ),
            },
            None => CheckItem {
                code: "ports".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "Порт {} занят другим процессом — старт бутылки с этим фиксированным \
                     портом откажет; освободите порт или смените его в профиле",
                    probe.port
                ),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bottle_core::profile::{
        BottleKind, BottleProfile, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig,
    };

    /// Ok-заглушка для аргумента-списка `wsl -l -q`.
    fn ok() -> io::Result<String> {
        Ok("ok".to_string())
    }

    fn env_profile_with_ports(id: &str, socks5: u16, listen: u16) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Бутылка {id}"),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: format!("{id}.conf"),
            },
            expose: ExposeConfig {
                socks5,
                forwards: vec![ForwardSpec {
                    listen,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: None,
                launch: None,
            },
        }
    }

    // ---------------------------------------------------------------
    // wsl: выключен / включён без дистрибуций / включён с дистрибуциями
    // / mirrored-режим сети.
    // ---------------------------------------------------------------

    #[test]
    fn wsl_disabled_is_fail_with_enable_hint() {
        let items = check_wsl(&Err(io::Error::other("wsl.exe не найден")), &ok(), &None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].code, "wsl");
        assert_eq!(items[0].status, CheckStatus::Fail);
        assert!(
            items[0].detail.contains("wsl --install"),
            "подсказка включения: {}",
            items[0].detail
        );
    }

    #[test]
    fn wsl_enabled_without_distributions_is_warn_portable_ok() {
        let items = check_wsl(&Ok("Version: 2.5".into()), &Ok(String::new()), &None);
        assert_eq!(items[0].status, CheckStatus::Warn);
        assert!(
            items[0].detail.contains("порт"),
            "пояснение, что это нормально: {}",
            items[0].detail
        );
    }

    #[test]
    fn wsl_enabled_with_distributions_is_ok() {
        let items = check_wsl(
            &Ok("Version: 2.5".into()),
            &Ok("Ubuntu\nbottleforapps".into()),
            &None,
        );
        assert_eq!(items[0].status, CheckStatus::Ok);
        assert!(items[0].detail.contains('2'), "число дистрибуций");
    }

    /// Спека §6: mirrored-режим сети WSL блокирует схему NAT —
    /// детектируется по .wslconfig (детерминированно, без локали).
    #[test]
    fn wsl_mirrored_networking_mode_is_fail_with_nat_hint() {
        let wslconfig = Some("[wsl2]\nnetworkingMode=mirrored\n".to_string());
        let items = check_wsl(
            &Ok("Version: 2.5".into()),
            &Ok("bottleforapps".into()),
            &wslconfig,
        );
        let mirrored = items.iter().find(|item| item.code == "wsl-networking");
        let item = mirrored.expect("отдельная строка про режим сети");
        assert_eq!(item.status, CheckStatus::Fail);
        assert!(
            item.detail.contains("NAT"),
            "подсказка возврата на NAT: {}",
            item.detail
        );
    }

    #[test]
    fn wsl_version_modern_store_wsl_is_ok_with_number() {
        let item = check_wsl_version(&Ok("Версия WSL: 2.5.7.0\nВерсия ядра: 5.15".into()));
        assert_eq!(item.code, "wsl-version");
        assert_eq!(item.status, CheckStatus::Ok);
        assert!(
            item.detail.contains("2.5.7.0"),
            "номер версии: {}",
            item.detail
        );
    }

    #[test]
    fn wsl_version_inbox_wsl_is_warn_not_fail() {
        let item = check_wsl_version(&Err(io::Error::other("Invalid command line option")));
        assert_eq!(item.code, "wsl-version");
        assert_eq!(item.status, CheckStatus::Warn);
        assert!(
            item.detail.contains("UTF-16LE"),
            "пояснение про декод: {}",
            item.detail
        );
    }

    #[test]
    fn wslconfig_without_mirrored_has_no_networking_item() {
        let wslconfig = Some("[wsl2]\nmemory=4GB\n".to_string());
        let items = check_wsl(
            &Ok("Version: 2.5".into()),
            &Ok("bottleforapps".into()),
            &wslconfig,
        );
        assert!(
            items.iter().all(|item| item.code != "wsl-networking"),
            "без mirrored — нет строки про сеть: {:?}",
            items
        );
    }

    // ---------------------------------------------------------------
    // appliance: не импортирован (норма до первого старта) /
    // импортирован с известной версией.
    // ---------------------------------------------------------------

    #[test]
    fn appliance_missing_is_warn_lazy_import() {
        let item = check_appliance(false, &None);
        assert_eq!(item.code, "appliance");
        assert_eq!(item.status, CheckStatus::Warn);
        assert!(
            item.detail.contains("первом старт"),
            "пояснение ленивого импорта: {}",
            item.detail
        );
    }

    #[test]
    fn appliance_imported_with_version_is_ok() {
        let item = check_appliance(true, &Some("0.2.0".to_string()));
        assert_eq!(item.status, CheckStatus::Ok);
        assert!(item.detail.contains("0.2.0"));
    }

    #[test]
    fn appliance_imported_without_session_is_ok() {
        let item = check_appliance(true, &None);
        assert_eq!(item.status, CheckStatus::Ok);
    }

    // ---------------------------------------------------------------
    // service: служба есть (host-бутылки доступны) / нет (env-only).
    // ---------------------------------------------------------------

    #[test]
    fn service_present_is_ok_host_bottles_available() {
        let item = check_service(true);
        assert_eq!(item.status, CheckStatus::Ok);
        assert!(item.detail.contains("host"));
    }

    #[test]
    fn service_missing_is_warn_env_only_with_install_hint() {
        let item = check_service(false);
        assert_eq!(item.status, CheckStatus::Warn);
        assert!(
            item.detail.contains("bottle-svc --install"),
            "подсказка установки: {}",
            item.detail
        );
    }

    // ---------------------------------------------------------------
    // agent: пассивная оценка (finding 2 финального ревью M3) —
    // doctor НЕ будит WSL: живая проба = ensure_appliance (импорт,
    // boot VM, 30 с бюджета) — побочный эффект и таймаут CLI на
    // чистой машине. Оценка по версии из handshake текущей сессии.
    // ---------------------------------------------------------------

    #[test]
    fn agent_connected_session_is_ok() {
        let item = check_agent(true, &Some("0.2.0".to_string()));
        assert_eq!(item.status, CheckStatus::Ok);
        assert!(item.detail.contains("0.2.0"));
    }

    #[test]
    fn agent_imported_but_not_connected_is_warn() {
        let item = check_agent(true, &None);
        assert_eq!(item.status, CheckStatus::Warn);
        assert!(
            item.detail.contains("первом старт"),
            "проверка отложена к первому старту: {}",
            item.detail
        );
    }

    #[test]
    fn agent_without_appliance_is_warn() {
        let item = check_agent(false, &None);
        assert_eq!(item.status, CheckStatus::Warn);
    }

    // ---------------------------------------------------------------
    // profiles: пусто / все читаются / есть битые.
    // ---------------------------------------------------------------

    #[test]
    fn profiles_empty_is_ok() {
        let item = check_profiles(&[]);
        assert_eq!(item.status, CheckStatus::Ok);
    }

    #[test]
    fn profiles_all_valid_is_ok_with_count() {
        let profile = env_profile_with_ports("office-a", 1080, 33389);
        let item = check_profiles(&[("office-a".into(), Ok(profile))]);
        assert_eq!(item.status, CheckStatus::Ok);
        assert!(item.detail.contains('1'));
    }

    #[test]
    fn profiles_broken_is_warn_with_ids() {
        let profile = env_profile_with_ports("office-a", 1080, 33389);
        let broken: Result<BottleProfile, DaemonError> = Err(DaemonError::IdMismatch {
            file_id: "broken".into(),
            profile_id: "other".into(),
        });
        let item = check_profiles(&[("office-a".into(), Ok(profile)), ("broken".into(), broken)]);
        assert_eq!(item.status, CheckStatus::Warn);
        assert!(
            item.detail.contains("broken"),
            "битый id назван: {}",
            item.detail
        );
    }

    // ---------------------------------------------------------------
    // ports: свободны / заняты чужим процессом / заняты живой бутылкой.
    // ---------------------------------------------------------------

    fn probe(port: u16, free: bool, live: Option<&str>) -> PortProbe {
        PortProbe {
            port,
            free,
            live_bottle: live.map(str::to_string),
        }
    }

    #[test]
    fn ports_without_fixed_ports_is_ok() {
        let items = check_ports(&[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, CheckStatus::Ok);
    }

    #[test]
    fn ports_all_free_is_ok() {
        let items = check_ports(&[probe(1080, true, None), probe(33389, true, None)]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, CheckStatus::Ok);
    }

    #[test]
    fn ports_busy_by_stranger_is_warn() {
        let items = check_ports(&[probe(1080, false, None)]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, CheckStatus::Warn);
        assert!(items[0].detail.contains("1080"));
    }

    #[test]
    fn ports_busy_by_live_bottle_is_ok() {
        let items = check_ports(&[probe(1080, false, Some("office-a"))]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, CheckStatus::Ok);
        assert!(items[0].detail.contains("office-a"));
    }
}
