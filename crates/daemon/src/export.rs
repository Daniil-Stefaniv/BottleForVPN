//! Экспорт журналов (`pogrebok logs export`): ZIP с журналами демона
//! (`<data>/logs`) и службы (`%PROGRAMDATA%\Pogrebok\logs`) и
//! сводкой `system.txt` — версии, состояния бутылок, профили без
//! содержимого конфигов (спека §10).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bottle_core::ipc::RuntimeState;
use bottle_core::profile::{BottleProfile, VpnConfig};
use bottle_core::protocol::PROTOCOL_VERSION;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::manager::BottleManager;

/// Каталог журналов службы внутри ZIP (второй источник журналов).
const SVC_LOGS_PREFIX: &str = "svc";

/// Собирает ZIP журналов: журнал демона `<data>/logs/*`, журнал службы
/// `%PROGRAMDATA%\Pogrebok\logs/*` (если есть), сводка
/// `system.txt`. `dest` — путь пользователя или имя по умолчанию в
/// каталоге данных. Возвращает путь к созданному архиву.
pub async fn export_logs(
    data_root: &Path,
    profiles: &[BottleProfile],
    manager: Option<&BottleManager>,
    dest: Option<PathBuf>,
) -> Result<PathBuf, io::Error> {
    let dest = dest.unwrap_or_else(|| default_dest(data_root, SystemTime::now()));
    let daemon_logs = data_root.join("logs");
    let runtime = match manager {
        Some(manager) => manager.status().await,
        None => Vec::new(),
    };
    let appliance_version = match manager {
        Some(manager) => manager.appliance_version().await,
        None => None,
    };

    let file = File::create(&dest)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    add_log_files(&mut zip, options, "logs", &daemon_logs)?;
    if let Some(svc_dir) = svc_logs_dir() {
        add_log_files(&mut zip, options, SVC_LOGS_PREFIX, &svc_dir)?;
    }

    zip.start_file("system.txt", options)?;
    zip.write_all(system_txt(profiles, appliance_version.as_deref(), &runtime).as_bytes())?;
    zip.finish()?;
    Ok(dest)
}

/// Добавляет в ZIP все файлы каталога `dir` под префиксом `prefix`
/// (плоско, без подкаталогей). Отсутствующий каталог — no-op: экспорт
/// не требует, чтобы служба когда-либо запускалась.
fn add_log_files<W: Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    options: SimpleFileOptions,
    prefix: &str,
    dir: &Path,
) -> Result<(), io::Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        zip.start_file(format!("{prefix}/{name}"), options)?;
        zip.write_all(&fs::read(&path)?)?;
    }
    Ok(())
}

/// Каталог журналов службы на этой машине (`%PROGRAMDATA%\Pogrebok\
/// logs`); `None` вне Windows/без PROGRAMDATA — журналов службы нет.
fn svc_logs_dir() -> Option<PathBuf> {
    let programdata = std::env::var_os("PROGRAMDATA")?;
    Some(Path::new(&programdata).join("Pogrebok").join("logs"))
}

/// Имя архива по умолчанию: `<data>/pogrebok-logs-<YYYYMMDD>-<HHMM>.zip`
/// (UTC — отметка информационная, локальная зона недоступна без
/// зависимостей).
fn default_dest(data_root: &Path, now: SystemTime) -> PathBuf {
    data_root.join(format!("pogrebok-logs-{}.zip", timestamp(now)))
}

/// Отметка времени `YYYYMMDD-HHMM` из unix-секунд (UTC); дата —
/// алгоритм Хиннанта (дни эпохи → григорианская дата).
fn timestamp(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (hour, minute) = (secs_of_day / 3600, (secs_of_day % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{hour:02}{minute:02}")
}

/// Сводка `system.txt`: версии демона/протокола/аплаенса, состояния
/// бутылок, профили. Содержимое конфигов не включается (секреты,
/// спека §10): env-профиль представлен метаданными, имя файла
/// конфига — не секрет.
fn system_txt(profiles: &[BottleProfile], appliance_version: Option<&str>, runtime: &[RuntimeState]) -> String {
    let mut out = String::new();
    out.push_str("Погребок — сводка системы\n");
    out.push_str(&format!("Версия демона: {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("Версия протокола: {PROTOCOL_VERSION}\n"));
    out.push_str(&format!(
        "Версия аплаенса: {}\n",
        appliance_version.unwrap_or("неизвестно (аплаенс не подключался в этой сессии)")
    ));
    out.push_str("\nСостояние бутылок:\n");
    if runtime.is_empty() {
        out.push_str("  (нет живых бутылок)\n");
    }
    for state in runtime {
        out.push_str(&format!(
            "  {}: {} (socks5: {})\n",
            state.id,
            state.state.as_str(),
            state
                .socks5
                .map(|port| port.to_string())
                .unwrap_or_else(|| "—".to_string())
        ));
    }
    out.push_str("\nПрофили:\n");
    for profile in profiles {
        out.push_str(&format!("  {}\n", profile_line(profile)));
    }
    out
}

/// Строка профиля в сводке: метаданные без содержимого конфига.
fn profile_line(profile: &BottleProfile) -> String {
    let vpn = match &profile.vpn {
        VpnConfig::Env { protocol, config } => {
            format!("kind=env protocol={protocol:?} config={config}")
        }
        VpnConfig::Host {
            client,
            args,
            endpoints,
            interface,
        } => format!(
            "kind=host client={client} args={} endpoints={} interface={}",
            crate::logging::mask_args(args.as_deref().unwrap_or("")),
            endpoints.join(","),
            interface.as_deref().unwrap_or("автоопределение")
        ),
    };
    format!(
        "{}: name={:?} {vpn} socks5={} forwards={} dns={}",
        profile.id,
        profile.name,
        profile.expose.socks5,
        profile.expose.forwards.len(),
        profile
            .expose
            .dns
            .map(|dns| dns.to_string())
            .unwrap_or_else(|| "—".to_string())
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use bottle_core::ipc::BottleState;
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig};

    /// Env-профиль с исходным конфигом `config` (до нормализации save).
    fn env_profile(id: &str, config: &Path) -> bottle_core::profile::BottleProfile {
        bottle_core::profile::BottleProfile {
            id: id.to_string(),
            name: format!("Бутылка {id}"),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: config.to_string_lossy().into_owned(),
            },
            expose: ExposeConfig {
                socks5: 1080,
                forwards: vec![ForwardSpec {
                    listen: 33389,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some("10.20.30.1".parse().unwrap()),
                launch: None,
            },
        }
    }

    /// Tempdir с каталогом данных `data/` и одним env-профилем;
    /// исходник конфига содержит маркер секрета.
    fn env_fixture() -> (tempfile::TempDir, PathBuf, Vec<BottleProfile>) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office.conf");
        fs::write(&src, "WG-SECRET-XYZ").unwrap();
        let data_root = tmp.path().join("data");
        fs::create_dir_all(&data_root).unwrap();
        let profiles = vec![env_profile("office-a", &src)];
        (tmp, data_root, profiles)
    }

    fn zip_entry_names(path: &Path) -> Vec<String> {
        let file = fs::File::open(path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect()
    }

    fn zip_entry_content(path: &Path, name: &str) -> String {
        let file = fs::File::open(path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut entry = zip.by_name(name).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    /// Имя по умолчанию: рядом с данными, с отметкой времени
    /// YYYYMMDD-HHMM (UTC) — без внешних зависимостей даты.
    #[test]
    fn default_dest_bears_data_root_and_utc_timestamp() {
        let epoch = UNIX_EPOCH + Duration::from_secs(0);
        assert_eq!(
            default_dest(Path::new("/data"), epoch),
            Path::new("/data").join("pogrebok-logs-19700101-0000.zip")
        );
        // 2026-10-04 12:34:56 UTC = 1791117296.
        let fixed = UNIX_EPOCH + Duration::from_secs(1_791_117_296);
        assert_eq!(
            default_dest(Path::new("/data"), fixed),
            Path::new("/data").join("pogrebok-logs-20261004-1234.zip")
        );
    }

    /// system.txt: версии и профили; содержимое конфигов (секреты,
    /// спека §10) не попадает.
    #[test]
    fn system_txt_lists_versions_and_profiles_without_config_secrets() {
        let (_tmp, _data_root, profiles) = env_fixture();

        let runtime = bottle_core::ipc::RuntimeState {
            id: "office-a".to_string(),
            state: BottleState::Up,
            socks5: Some(1080),
            forwards: vec![bottle_core::protocol::ForwardPort {
                port: 33389,
                target: "10.20.30.15:3389".to_string(),
            }],
        };
        let text = system_txt(&profiles, None, std::slice::from_ref(&runtime));

        assert!(text.contains("office-a"), "профиль перечислен: {text}");
        assert!(text.contains("WireGuard"), "протокол профиля: {text}");
        assert!(text.contains("up"), "состояние бутылки: {text}");
        assert!(
            !text.contains("WG-SECRET-XYZ"),
            "содержимое конфига — секрет, в сводке его нет: {text}"
        );
    }

    /// Экспорт в заданный путь: ZIP читается, содержит журнал демона
    /// и system.txt.
    #[tokio::test]
    async fn export_zip_contains_daemon_logs_and_system_txt() {
        let (tmp, data_root, profiles) = env_fixture();
        let logs_dir = data_root.join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        fs::write(logs_dir.join("pogrebok-daemon.1970-01-01"), "строка журнала").unwrap();

        let dest = tmp.path().join("export.zip");
        let result = export_logs(&data_root, &profiles, None, Some(dest.clone())).await;

        let path = result.expect("экспорт без службы и менеджера успешен");
        assert_eq!(path, dest, "использован путь пользователя");
        let names = zip_entry_names(&dest);
        assert!(
            names.contains(&"logs/pogrebok-daemon.1970-01-01".to_string()),
            "журнал демона внутри: {names:?}"
        );
        assert!(
            names.contains(&"system.txt".to_string()),
            "сводка внутри: {names:?}"
        );
        assert_eq!(
            zip_entry_content(&dest, "logs/pogrebok-daemon.1970-01-01"),
            "строка журнала"
        );
    }

    /// Экспорт без каталога журналов не падает: ZIP с одной сводкой.
    #[tokio::test]
    async fn export_without_log_dirs_creates_zip_with_system_txt_only() {
        let (tmp, data_root, profiles) = env_fixture();

        let dest = tmp.path().join("export.zip");
        export_logs(&data_root, &profiles, None, Some(dest.clone()))
            .await
            .expect("отсутствие журналов — не ошибка");

        let names = zip_entry_names(&dest);
        assert_eq!(names, vec!["system.txt".to_string()], "только сводка");
    }

    /// Экспорт без пути кладёт ZIP рядом с данными с отметкой времени.
    #[tokio::test]
    async fn export_without_dest_uses_default_name_in_data_root() {
        let (_tmp, data_root, profiles) = env_fixture();

        let path = export_logs(&data_root, &profiles, None, None)
            .await
            .expect("экспорт");

        assert!(
            path.starts_with(&data_root),
            "ZIP в каталоге данных: {}",
            path.display()
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pogrebok-logs-"),
            "имя с отметкой: {}",
            path.display()
        );
    }
}
