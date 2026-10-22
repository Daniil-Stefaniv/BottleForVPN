//! CRUD бутылок поверх [`Vault`] (0.3, спека §4): профили, тексты
//! конфигов и креды — в зашифрованной БД. `vpn_json`/`expose_json` —
//! serde_json-снапшоты существующих структур `bottle_core::profile`
//! (параллельной модели нет). Мутации — транзакции (бутылка + конфиг +
//! креды атомарно).

use bottle_core::ipc::{BottleSummary, FullProfile, VpnCredentials};
use bottle_core::profile::{BottleKind, BottleProfile, ExposeConfig, VpnConfig};

use super::Vault;

impl Vault {
    /// Сводки всех бутылок (вкладки «Env»/«Host» главного окна).
    /// `state` заполняет фронт из статуса демона — здесь `None`.
    /// Конфиги (`configs.config_text`) здесь не читаются: список
    /// остаётся быстрым при мегабайтных конфигах (Review Focus #4).
    pub fn list_bottles(&self) -> Result<Vec<BottleSummary>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, kind FROM bottles ORDER BY id")
            .map_err(|e| format!("не удалось прочитать бутылки: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| format!("не удалось прочитать бутылки: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, kind) = row.map_err(|e| format!("битая строка bottles: {e}"))?;
            out.push(BottleSummary {
                id,
                name,
                kind: parse_kind(&kind)?,
                state: None,
            });
        }
        Ok(out)
    }

    /// Полный профиль бутылки (профиль + текст конфига + креды);
    /// `None` — нет такой.
    pub fn get_full(&self, id: &str) -> Result<Option<FullProfile>, String> {
        let row = self
            .conn
            .query_row(
                "SELECT b.id, b.name, b.kind, b.vpn_json, b.expose_json,
                        c.config_text, r.username, r.password
                 FROM bottles b
                 LEFT JOIN configs c ON c.bottle_id = b.id
                 LEFT JOIN credentials r ON r.bottle_id = b.id
                 WHERE b.id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            );
        let (id, name, kind, vpn_json, expose_json, config_text, username, password) = match row {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(format!("не удалось прочитать бутылку {id}: {e}")),
        };
        Ok(Some(assemble_full(
            id,
            name,
            &kind,
            &vpn_json,
            &expose_json,
            config_text,
            username,
            password,
        )?))
    }

    /// Полные профили всех бутылок (SyncProfiles демону при старте GUI).
    pub fn all_full(&self) -> Result<Vec<FullProfile>, String> {
        let ids: Vec<String> = self
            .list_bottles()?
            .into_iter()
            .map(|summary| summary.id)
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(full) = self.get_full(&id)? {
                out.push(full);
            }
        }
        Ok(out)
    }

    /// Создаёт бутылку (профиль + конфиг + креды одной транзакцией).
    /// Дубль id — понятная ошибка, БД не тронута (Review Focus #5).
    pub fn create_bottle(&self, p: &FullProfile) -> Result<(), String> {
        if self.get_full(&p.profile.id)?.is_some() {
            return Err(format!(
                "бутылка с id «{}» уже существует — выберите другой id",
                p.profile.id
            ));
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| format!("транзакция не открылась: {e}"))?;
        write_profile(&tx, p)?;
        tx.commit()
            .map_err(|e| format!("не удалось сохранить бутылку: {e}"))
    }

    /// Обновляет бутылку (профиль + конфиг; креды — только если переданы
    /// в `p.credentials`: `None` оставляет сохранённые нетронутыми).
    /// Несуществующий id — ошибка.
    pub fn update_bottle(&self, p: &FullProfile) -> Result<(), String> {
        if self.get_full(&p.profile.id)?.is_none() {
            return Err(format!("бутылка не найдена: {}", p.profile.id));
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| format!("транзакция не открылась: {e}"))?;
        write_profile(&tx, p)?;
        tx.commit()
            .map_err(|e| format!("не удалось обновить бутылку: {e}"))
    }

    /// Удаляет бутылку; конфиг и креды уходят каскадом.
    /// Несуществующий id — ошибка.
    pub fn delete_bottle(&self, id: &str) -> Result<(), String> {
        let affected = self
            .conn
            .execute("DELETE FROM bottles WHERE id = ?1", [id])
            .map_err(|e| format!("не удалось удалить бутылку {id}: {e}"))?;
        if affected == 0 {
            return Err(format!("бутылка не найдена: {id}"));
        }
        Ok(())
    }

    /// Сохраняет логин/пароль бутылки (кнопка «Логин и пароль»).
    /// Бутылка должна существовать.
    pub fn set_credentials(&self, id: &str, user: &str, pass: &str) -> Result<(), String> {
        if self.get_full(id)?.is_none() {
            return Err(format!("бутылка не найдена: {id}"));
        }
        self.conn
            .execute(
                "INSERT INTO credentials (bottle_id, username, password, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (bottle_id) DO UPDATE SET
                    username = excluded.username,
                    password = excluded.password,
                    updated_at = excluded.updated_at",
                rusqlite::params![id, user, pass, super::now_utc()],
            )
            .map_err(|e| format!("не удалось сохранить логин и пароль: {e}"))?;
        Ok(())
    }

    /// Сохранённые логин/пароль бутылки; `None` — не заданы.
    pub fn get_credentials(&self, id: &str) -> Result<Option<(String, String)>, String> {
        match self.conn.query_row(
            "SELECT username, password FROM credentials WHERE bottle_id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            Ok(pair) => Ok(Some(pair)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("не удалось прочитать креды {id}: {e}")),
        }
    }
}

/// Записывает профиль + конфиг + креды внутри открытой транзакции
/// (create и update — один и тот же upsert-путь).
fn write_profile(
    tx: &rusqlite::Transaction<'_>,
    p: &FullProfile,
) -> Result<(), String> {
    let profile = &p.profile;
    let kind = kind_str(profile.kind);
    let vpn_json = serde_json::to_string(&profile.vpn)
        .map_err(|e| format!("vpn не сериализуется: {e}"))?;
    let expose_json = serde_json::to_string(&profile.expose)
        .map_err(|e| format!("expose не сериализуется: {e}"))?;
    let now = super::now_utc();
    tx.execute(
        "INSERT INTO bottles (id, name, kind, vpn_json, expose_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT (id) DO UPDATE SET
            name = excluded.name,
            kind = excluded.kind,
            vpn_json = excluded.vpn_json,
            expose_json = excluded.expose_json,
            updated_at = excluded.updated_at",
        rusqlite::params![profile.id, profile.name, kind, vpn_json, expose_json, now],
    )
    .map_err(|e| format!("не удалось записать профиль: {e}"))?;
    match &p.config_text {
        Some(text) => {
            tx.execute(
                "INSERT INTO configs (bottle_id, config_text) VALUES (?1, ?2)
                 ON CONFLICT (bottle_id) DO UPDATE SET config_text = excluded.config_text",
                rusqlite::params![profile.id, text],
            )
            .map_err(|e| format!("не удалось записать конфиг: {e}"))?;
        }
        None => {
            // Конфиг снят (host-бутылка после правки из env): строка
            // не должна остаться призраком.
            tx.execute(
                "DELETE FROM configs WHERE bottle_id = ?1",
                [&profile.id],
            )
            .map_err(|e| format!("не удалось снять конфиг: {e}"))?;
        }
    }
    if let Some(creds) = &p.credentials {
        tx.execute(
            "INSERT INTO credentials (bottle_id, username, password, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (bottle_id) DO UPDATE SET
                username = excluded.username,
                password = excluded.password,
                updated_at = excluded.updated_at",
            rusqlite::params![profile.id, creds.username, creds.password, now],
        )
        .map_err(|e| format!("не удалось записать креды: {e}"))?;
    }
    Ok(())
}

/// Собирает FullProfile из строки join'а; JSON-поля обязаны разбираться
/// (писали их мы сами — битое значение = повреждённая БД).
#[allow(clippy::too_many_arguments)]
fn assemble_full(
    id: String,
    name: String,
    kind: &str,
    vpn_json: &str,
    expose_json: &str,
    config_text: Option<String>,
    username: Option<String>,
    password: Option<String>,
) -> Result<FullProfile, String> {
    let vpn: VpnConfig = serde_json::from_str(vpn_json)
        .map_err(|e| format!("битый vpn_json бутылки {id}: {e}"))?;
    let expose: ExposeConfig = serde_json::from_str(expose_json)
        .map_err(|e| format!("битый expose_json бутылки {id}: {e}"))?;
    Ok(FullProfile {
        profile: BottleProfile {
            id,
            name,
            kind: parse_kind(kind)?,
            vpn,
            expose,
        },
        config_text,
        credentials: match (username, password) {
            (Some(username), Some(password)) => Some(VpnCredentials { username, password }),
            _ => None,
        },
    })
}

/// kind в БД — serde-имя (`env`/`host`, совпадает с CHECK-ограничением).
fn kind_str(kind: BottleKind) -> &'static str {
    match kind {
        BottleKind::Env => "env",
        BottleKind::Host => "host",
    }
}

/// Разбор kind из БД; постороннее значение — повреждённая БД.
fn parse_kind(kind: &str) -> Result<BottleKind, String> {
    match kind {
        "env" => Ok(BottleKind::Env),
        "host" => Ok(BottleKind::Host),
        other => Err(format!("неизвестный тип бутылки в БД: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bottle_core::profile::{ForwardSpec, TunnelProtocol};

    fn env_full(id: &str) -> FullProfile {
        FullProfile {
            profile: BottleProfile {
                id: id.to_string(),
                name: format!("Офис {id}"),
                kind: BottleKind::Env,
                vpn: VpnConfig::Env {
                    protocol: TunnelProtocol::WireGuard,
                    config: format!("{id}.conf"),
                },
                expose: ExposeConfig {
                    socks5: 1080,
                    forwards: vec![ForwardSpec {
                        listen: 33389,
                        target: "10.0.0.15:3389".to_string(),
                    }],
                    dns: Some("10.0.0.1".parse().unwrap()),
                    launch: Some("mstsc".to_string()),
                },
            },
            config_text: Some("[Interface]\nPrivateKey = SECRET\n".to_string()),
            credentials: Some(VpnCredentials {
                username: "admin".to_string(),
                password: "s3cr3t 🍾".to_string(),
            }),
        }
    }

    fn host_full(id: &str) -> FullProfile {
        FullProfile {
            profile: BottleProfile {
                id: id.to_string(),
                name: format!("Хост {id}"),
                kind: BottleKind::Host,
                vpn: VpnConfig::Host {
                    client: r"C:\vpn.exe".to_string(),
                    args: Some("connect".to_string()),
                    endpoints: vec!["vpn.example.com".to_string()],
                    interface: None,
                },
                expose: ExposeConfig {
                    socks5: 0,
                    forwards: vec![],
                    dns: None,
                    launch: None,
                },
            },
            config_text: None,
            credentials: None,
        }
    }

    fn vault() -> (tempfile::TempDir, Vault) {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::create(&tmp.path().join(crate::db::VAULT_FILE_NAME), "pw").unwrap();
        (tmp, vault)
    }

    /// Полный круг: env и host профили с конфигом и кредами
    /// восстанавливаются из БД без потерь.
    #[test]
    fn create_get_round_trip_with_config_and_creds() {
        let (_tmp, vault) = vault();
        vault.create_bottle(&env_full("office-a")).unwrap();
        vault.create_bottle(&host_full("corp")).unwrap();

        let env = vault.get_full("office-a").unwrap().expect("env есть");
        assert_eq!(env, env_full("office-a"));
        let host = vault.get_full("corp").unwrap().expect("host есть");
        assert_eq!(host, host_full("corp"));
        assert!(vault.get_full("ghost").unwrap().is_none());

        let list = vault.list_bottles().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "corp", "сортировка по id");
        assert_eq!(list[0].kind, BottleKind::Host);
        assert_eq!(list[1].kind, BottleKind::Env);
        assert!(list.iter().all(|s| s.state.is_none()));

        let all = vault.all_full().unwrap();
        assert_eq!(all.len(), 2);
    }

    /// Дубль id — понятная ошибка, существующая запись не тронута
    /// (Review Focus #5).
    #[test]
    fn duplicate_id_is_error_and_db_untouched() {
        let (_tmp, vault) = vault();
        vault.create_bottle(&env_full("office-a")).unwrap();

        let mut dup = env_full("office-a");
        dup.profile.name = "Подмена".to_string();
        let error = vault.create_bottle(&dup).unwrap_err();
        assert!(error.contains("уже существует"), "{error}");

        let stored = vault.get_full("office-a").unwrap().unwrap();
        assert_eq!(stored.profile.name, "Офис office-a", "запись не тронута");
    }

    /// Мегабайтный конфиг (Review Focus #4): round-trip без потерь,
    /// list_bottles не читает config_text (остаётся быстрым).
    #[test]
    fn megabyte_config_text_round_trip() {
        let (_tmp, vault) = vault();
        let mut big = env_full("big");
        big.config_text = Some("x".repeat(1_000_000));
        vault.create_bottle(&big).unwrap();

        let loaded = vault.get_full("big").unwrap().unwrap();
        assert_eq!(loaded.config_text.as_deref().map(str::len), Some(1_000_000));

        let start = std::time::Instant::now();
        let list = vault.list_bottles().unwrap();
        assert_eq!(list.len(), 1);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "список не читает мегабайтные конфиги: {:?}",
            start.elapsed()
        );
    }

    /// Удаление бутылки уносит конфиг и креды каскадом.
    #[test]
    fn delete_cascades_config_and_creds() {
        let (_tmp, vault) = vault();
        vault.create_bottle(&env_full("office-a")).unwrap();
        vault.delete_bottle("office-a").unwrap();

        assert!(vault.get_full("office-a").unwrap().is_none());
        assert!(vault.get_credentials("office-a").unwrap().is_none());
        assert!(vault.list_bottles().unwrap().is_empty());
        assert!(vault.delete_bottle("office-a").unwrap_err().contains("не найдена"));
    }

    /// update_bottle: профиль и конфиг перезаписываются; credentials:
    /// None — сохранённые не трогаются (правка профиля не стирает креды).
    #[test]
    fn update_rewrites_profile_and_keeps_credentials_unless_passed() {
        let (_tmp, vault) = vault();
        vault.create_bottle(&env_full("office-a")).unwrap();

        let mut updated = env_full("office-a");
        updated.profile.name = "Переименованная".to_string();
        updated.credentials = None;
        vault.update_bottle(&updated).unwrap();

        let stored = vault.get_full("office-a").unwrap().unwrap();
        assert_eq!(stored.profile.name, "Переименованная");
        assert_eq!(
            stored.credentials,
            Some(VpnCredentials {
                username: "admin".to_string(),
                password: "s3cr3t 🍾".to_string()
            }),
            "креды пережили правку профиля"
        );

        assert!(vault
            .update_bottle(&env_full("ghost"))
            .unwrap_err()
            .contains("не найдена"));
    }

    /// set_credentials: upsert поверх существующих; отдельное чтение.
    #[test]
    fn credentials_upsert_and_read() {
        let (_tmp, vault) = vault();
        vault.create_bottle(&host_full("corp")).unwrap();
        assert!(vault.get_credentials("corp").unwrap().is_none());

        vault.set_credentials("corp", "user1", "pass1").unwrap();
        vault.set_credentials("corp", "user2", "пароль 🍾").unwrap();
        assert_eq!(
            vault.get_credentials("corp").unwrap(),
            Some(("user2".to_string(), "пароль 🍾".to_string()))
        );

        assert!(vault
            .set_credentials("ghost", "u", "p")
            .unwrap_err()
            .contains("не найдена"));
    }
}
