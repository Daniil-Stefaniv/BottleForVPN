//! Хранилище профилей бутылок: `<root>/bottles/<id>.toml` и копии
//! конфигов VPN `<root>/bottles/<id>.conf` (секреты, права 0600).
//! Корень хранилища — 0700 (спека §10: ACL только на пользователя).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bottle_core::profile::{parse_profile, render_profile, validate_id, BottleProfile, VpnConfig};

use crate::error::DaemonError;

/// Хранилище профилей в каталоге `root`.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Открывает (создаёт при необходимости) хранилище в `root`:
    /// каталоги `bottles/` и `logs/`. На unix корень получает права 0700 —
    /// в хранилище лежат секреты (токен appliance, копии VPN-конфигов,
    /// спека §10); уже существующий каталог с другими правами тоже
    /// ужесточается до 0700.
    pub fn open(root: &Path) -> io::Result<Store> {
        fs::create_dir_all(root)?;
        fs::create_dir_all(root.join("bottles"))?;
        fs::create_dir_all(root.join("logs"))?;
        set_private(root)?;
        Ok(Store {
            root: root.to_path_buf(),
        })
    }

    /// Путь к файлу токена appliance (`root/appliance-token`).
    pub fn appliance_token_path(&self) -> PathBuf {
        self.root.join("appliance-token")
    }

    /// Корень данных демона.
    pub fn data_root(&self) -> &Path {
        &self.root
    }

    /// Все профили из `bottles/*.toml`, отсортированные по id.
    /// Битый файл (разбор, несовпадение id с именем файла) портит только
    /// свой элемент, не весь список. Каталог создаётся [`Store::open`],
    /// поэтому недоступность `bottles/` трактуется как пустой список.
    pub fn list(&self) -> Vec<(String, Result<BottleProfile, DaemonError>)> {
        let mut items = Vec::new();
        let Ok(entries) = fs::read_dir(self.root.join("bottles")) else {
            return items;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let id = stem.to_string();
            let result = read_profile(&path, &id);
            items.push((id, result));
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    /// Читает и валидирует профиль `bottles/<id>.toml`.
    ///
    /// `id` сначала проверяется `validate_id` — подозрительные значения
    /// (`../evil`) отвергаются до выхода за пределы корня хранилища.
    pub fn load(&self, id: &str) -> Result<BottleProfile, DaemonError> {
        validate_id(id)?;
        read_profile(&self.profile_path(id), id)
    }

    /// Атомарно сохраняет профиль в `bottles/<id>.toml`.
    ///
    /// Для `VpnConfig::Env` поле `config` трактуется как путь к исходному
    /// конфигу: файл копируется в `bottles/<id>.conf` (права 0600), а в
    /// сохраняемом профиле `config` нормализуется в `<id>.conf`. Если
    /// `config` уже равен `<id>.conf` (профиль из [`Store::load`]), копия
    /// не выполняется — используется уже лежащая в хранилище.
    pub fn save(&self, p: &BottleProfile) -> Result<(), DaemonError> {
        self.save_inner(p, None)
    }

    /// Как [`Store::save`], но конфиг не копируется из исходного пути —
    /// в `bottles/<id>.conf` записывается готовый текст (например, OpenVPN
    /// после инлайнинга внешних файлов — [`inline_openvpn_config`]).
    pub fn save_with_config_text(&self, p: &BottleProfile, text: &str) -> Result<(), DaemonError> {
        self.save_inner(p, Some(text))
    }

    fn save_inner(&self, p: &BottleProfile, config_text: Option<&str>) -> Result<(), DaemonError> {
        validate_id(&p.id)?;

        let mut stored = p.clone();
        if let VpnConfig::Env { config, .. } = &stored.vpn {
            let conf_name = format!("{}.conf", p.id);
            let conf_path = self.root.join("bottles").join(&conf_name);
            if let Some(text) = config_text {
                atomic_write(&conf_path, text.as_bytes())?;
            } else if config != &conf_name {
                copy_config(Path::new(config), &conf_path)?;
            } else if !conf_path.exists() {
                return Err(DaemonError::SourceConfigMissing { path: conf_path });
            }
            if let VpnConfig::Env { config, .. } = &mut stored.vpn {
                *config = conf_name;
            }
        }

        let text = render_profile(&stored)?;
        atomic_write(&self.profile_path(&p.id), text.as_bytes())?;
        Ok(())
    }

    /// Путь до профиля: `bottles/<id>.toml`.
    fn profile_path(&self, id: &str) -> PathBuf {
        self.root.join("bottles").join(format!("{id}.toml"))
    }

    /// Удаляет профиль и копию конфига (`bottles/<id>.toml` и
    /// `bottles/<id>.conf`); возвращает, существовал ли файл конфига
    /// (отсутствующий конфиг — предупреждение, не ошибка). Профиль
    /// обязан существовать — иначе [`DaemonError::NotFound`].
    pub fn delete(&self, id: &str) -> Result<bool, DaemonError> {
        validate_id(id)?;
        let toml_path = self.profile_path(id);
        if !toml_path.exists() {
            return Err(DaemonError::NotFound { id: id.to_string() });
        }
        let conf_path = self.root.join("bottles").join(format!("{id}.conf"));
        let conf_existed = conf_path.exists();
        fs::remove_file(&toml_path)?;
        if conf_existed {
            fs::remove_file(&conf_path)?;
        }
        Ok(conf_existed)
    }
}

/// Читает, разбирает и сверяет профиль в `path` с ожидаемым id файла.
fn read_profile(path: &Path, file_id: &str) -> Result<BottleProfile, DaemonError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(DaemonError::NotFound {
                id: file_id.to_string(),
            })
        }
        Err(e) => return Err(e.into()),
    };
    let profile = parse_profile(&text)?;
    if profile.id != file_id {
        return Err(DaemonError::IdMismatch {
            file_id: file_id.to_string(),
            profile_id: profile.id,
        });
    }
    Ok(profile)
}

/// Копирует конфиг VPN в хранилище как UTF-8 текст (права 0600).
/// Отсутствующий исходник — точная ошибка, а не общий io::Error.
fn copy_config(source: &Path, target: &Path) -> Result<(), DaemonError> {
    let text = match fs::read_to_string(source) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(DaemonError::SourceConfigMissing {
                path: source.to_path_buf(),
            })
        }
        // В том числе невалидный UTF-8 (ErrorKind::InvalidData).
        Err(e) => return Err(e.into()),
    };
    atomic_write(target, text.as_bytes())?;
    Ok(())
}

/// Атомарная запись: сначала временный файл рядом с целью, затем rename.
/// Существующий файл не портится при отказе на любом шаге; временный
/// файл при ошибке удаляется.
///
/// Используется и за пределами хранилища профилей (токен аплаенса,
/// `crate::appliance`), поэтому видима внутри крейта.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));

    let write = || -> io::Result<()> {
        let mut file = create_private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Создаёт файл с правами 0600 на unix (без окна с правами по umask);
/// на других платформах — обычное создание.
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::File::create(path)
}

/// Права 0700 на корень хранилища (unix); на других платформах — no-op.
#[cfg(unix)]
fn set_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bottle_core::profile::{
        BottleKind, ExposeConfig, ForwardSpec, ProfileError, TunnelProtocol,
    };

    /// Env-профиль с исходным конфигом `config` (путь до нормализации).
    fn env_profile(id: &str, config: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Бутылка {id}"),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: config.to_string(),
            },
            expose: ExposeConfig {
                socks5: 0,
                forwards: vec![ForwardSpec {
                    listen: 0,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some("10.20.30.1".parse().unwrap()),
                launch: Some("mstsc".to_string()),
            },
        }
    }

    /// Host-профиль со всеми опциональными полями (round-trip Host из T4).
    fn host_profile(id: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Хост {id}"),
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

    /// Ожидаемый вид env-профиля после save: `config` нормализован.
    fn normalized_env(p: &BottleProfile) -> BottleProfile {
        let mut stored = p.clone();
        if let VpnConfig::Env { config, .. } = &mut stored.vpn {
            *config = format!("{}.conf", p.id);
        }
        stored
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn open_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();

        assert!(root.is_dir());
        assert!(root.join("bottles").is_dir());
        assert!(root.join("logs").is_dir());
        assert_eq!(store.data_root(), root.as_path());
        assert_eq!(store.appliance_token_path(), root.join("appliance-token"));
    }

    #[cfg(unix)]
    #[test]
    fn open_makes_root_private_even_if_it_existed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        Store::open(&root).unwrap();

        assert_eq!(mode(&root), 0o700);
    }

    #[test]
    fn save_env_copies_config_and_load_normalizes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office-a.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        let p = env_profile("office-a", src.to_str().unwrap());

        store.save(&p).unwrap();

        let conf = root.join("bottles").join("office-a.conf");
        assert_eq!(fs::read_to_string(&conf).unwrap(), "wg-secret");
        #[cfg(unix)]
        assert_eq!(mode(&conf), 0o600, "копия конфига — секрет, права 0600");

        let loaded = store.load("office-a").unwrap();
        assert_eq!(loaded, normalized_env(&p));

        // Исходник остался на месте: копия, не перенос.
        assert_eq!(fs::read_to_string(&src).unwrap(), "wg-secret");
    }

    #[test]
    fn resave_of_loaded_profile_keeps_conf_and_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-a.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store
            .save(&env_profile("a", src.to_str().unwrap()))
            .unwrap();

        // У загруженного профиля config уже нормализован ("a.conf") —
        // повторный save не должен искать такой файл снаружи хранилища.
        let loaded = store.load("a").unwrap();
        store.save(&loaded).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("bottles").join("a.conf")).unwrap(),
            "wg-secret"
        );
        assert_eq!(store.load("a").unwrap(), loaded);
    }

    #[test]
    fn list_after_two_saves_is_sorted_and_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store.save(&host_profile("corp-vpn")).unwrap();
        store
            .save(&env_profile("office", src.to_str().unwrap()))
            .unwrap();

        let items = store.list();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "corp-vpn");
        assert_eq!(items[0].1.as_ref().unwrap(), &host_profile("corp-vpn"));
        assert_eq!(items[1].0, "office");
        assert_eq!(
            items[1].1.as_ref().unwrap(),
            &normalized_env(&env_profile("office", ""))
        );
    }

    #[test]
    fn load_missing_profile_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();

        match store.load("ghost") {
            Err(DaemonError::NotFound { id }) => assert_eq!(id, "ghost"),
            other => panic!("ожидали NotFound, получили {other:?}"),
        }
    }

    #[test]
    fn list_reports_mismatched_id_file_and_keeps_neighbours() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store.save(&host_profile("aaa")).unwrap();
        // Валидный TOML, но поле id не совпадает с именем файла.
        fs::write(
            root.join("bottles").join("mismatch.toml"),
            r#"
id = "other"
name = "Чужой"
kind = "host"

[vpn]
client = "vpn.exe"
endpoints = ["e.example"]
"#,
        )
        .unwrap();

        let items = store.list();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "aaa");
        assert!(items[0].1.is_ok());
        assert_eq!(items[1].0, "mismatch");
        match &items[1].1 {
            Err(DaemonError::IdMismatch {
                file_id,
                profile_id,
            }) => {
                assert_eq!(
                    (file_id.as_str(), profile_id.as_str()),
                    ("mismatch", "other")
                );
            }
            other => panic!("ожидали IdMismatch, получили {other:?}"),
        }
    }

    #[test]
    fn load_rejects_path_traversal_before_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();

        // Ошибка валидации, а не NotFound: подозрительный id отвергается
        // до какого-либо обращения к файловой системе за пределами root.
        match store.load("../evil") {
            Err(DaemonError::Profile(ProfileError::InvalidId)) => {}
            other => panic!("ожидали Profile(InvalidId), получили {other:?}"),
        }
    }

    #[test]
    fn save_with_missing_source_config_fails_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        let missing = tmp.path().join("no-such.conf");

        let err = store
            .save(&env_profile("office", missing.to_str().unwrap()))
            .unwrap_err();

        match &err {
            DaemonError::SourceConfigMissing { path } => assert_eq!(*path, missing),
            other => panic!("ожидали SourceConfigMissing, получили {other:?}"),
        }
        // Профиль не создан: сначала конфиг, потом запись TOML.
        assert!(!root.join("bottles").join("office.toml").exists());
    }

    /// Удаление env-профиля сносит и TOML, и копию конфига.
    #[test]
    fn delete_removes_toml_and_conf() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-a.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store
            .save(&env_profile("a", src.to_str().unwrap()))
            .unwrap();

        let conf_existed = store.delete("a").unwrap();

        assert!(conf_existed, "конфиг существовал до удаления");
        assert!(!root.join("bottles").join("a.toml").exists());
        assert!(!root.join("bottles").join("a.conf").exists());
        assert!(matches!(store.load("a"), Err(DaemonError::NotFound { .. })));
    }

    /// Удаление host-профиля (без конфига): TOML удалён, конфига не было.
    #[test]
    fn delete_without_conf_reports_missing_conf() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store.save(&host_profile("corp")).unwrap();

        let conf_existed = store.delete("corp").unwrap();

        assert!(!conf_existed, "конфига у host-профиля нет");
        assert!(!root.join("bottles").join("corp.toml").exists());
    }

    /// Удаление отсутствующего профиля — NotFound.
    #[test]
    fn delete_missing_profile_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();

        assert!(matches!(
            store.delete("ghost"),
            Err(DaemonError::NotFound { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failed_save_leaves_existing_profile_intact() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-a.conf");
        fs::write(&src, "wg-secret").unwrap();
        let root = tmp.path().join("store");
        let store = Store::open(&root).unwrap();
        store
            .save(&env_profile("a", src.to_str().unwrap()))
            .unwrap();
        let toml_path = root.join("bottles").join("a.toml");
        let before = fs::read_to_string(&toml_path).unwrap();

        // render_profile для валидного BottleProfile упасть не может
        // (toml экранирует любые строки), поэтому отказ инъектируем
        // на этапе записи временного файла: каталог bottles только
        // для чтения. Инвариант тот же: существующий профиль не портится.
        let loaded = store.load("a").unwrap();
        let dir_perms = fs::metadata(root.join("bottles")).unwrap().permissions();
        let mut ro = dir_perms.clone();
        ro.set_mode(0o555);
        fs::set_permissions(root.join("bottles"), ro).unwrap();
        let err = store.save(&loaded).unwrap_err();
        fs::set_permissions(root.join("bottles"), dir_perms).unwrap();

        assert!(
            matches!(err, DaemonError::Io(_)),
            "ожидали Io, получили {err:?}"
        );
        assert_eq!(fs::read_to_string(&toml_path).unwrap(), before);
        assert!(
            !root.join("bottles").join("a.toml.tmp").exists(),
            "временный файл убран"
        );
    }
}
