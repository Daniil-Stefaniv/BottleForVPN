//! Реестр профилей демона (0.3, спека §2): единственное «хранилище»
//! демона — память. Профили, тексты конфигов и креды приходят от GUI
//! сообщениями SyncProfiles/UpsertProfile/RemoveProfile; дискового
//! хранилища у демона больше нет.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bottle_core::ipc::FullProfile;

/// Разделяемый реестр: диспетчер pipe пишет, менеджер бутылок читает.
pub type SharedRegistry = Arc<RwLock<Registry>>;

/// In-memory реестр профилей по id бутылки.
#[derive(Default)]
pub struct Registry {
    profiles: HashMap<String, FullProfile>,
}

impl Registry {
    /// Пустой реестр в разделяемой обёртке.
    pub fn new_shared() -> SharedRegistry {
        Arc::new(RwLock::new(Registry::default()))
    }

    /// Полная замена содержимого (SyncProfiles от GUI): атомарно,
    /// промежуточного «полупустого» состояния читатели не видят.
    pub fn replace_all(&mut self, profiles: Vec<FullProfile>) {
        self.profiles = profiles
            .into_iter()
            .map(|p| (p.profile.id.clone(), p))
            .collect();
    }

    /// Добавление или замена одного профиля (UpsertProfile).
    pub fn upsert(&mut self, p: FullProfile) {
        self.profiles.insert(p.profile.id.clone(), p);
    }

    /// Удаление профиля; `false` — такого id не было.
    pub fn remove(&mut self, id: &str) -> bool {
        self.profiles.remove(id).is_some()
    }

    /// Профиль по id (клон: конфиги ≤ ~1 МБ, старт бутылки редок).
    pub fn get(&self, id: &str) -> Option<FullProfile> {
        self.profiles.get(id).cloned()
    }

    /// Отсортированный список id.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.profiles.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Число профилей в реестре.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Пуст ли реестр (демон ещё не получил SyncProfiles).
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use bottle_core::ipc::FullProfile;
    use bottle_core::profile::{
        BottleKind, BottleProfile, ExposeConfig, TunnelProtocol, VpnConfig,
    };

    use super::*;

    fn profile_fixture(id: &str) -> FullProfile {
        FullProfile {
            profile: BottleProfile {
                id: id.to_string(),
                name: format!("Бутылка {id}"),
                kind: BottleKind::Env,
                vpn: VpnConfig::Env {
                    protocol: TunnelProtocol::WireGuard,
                    config: format!("{id}.conf"),
                },
                expose: ExposeConfig {
                    socks5: 1080,
                    forwards: vec![],
                    dns: Some(IpAddr::from([10, 0, 0, 1])),
                    launch: None,
                },
            },
            config_text: Some("c".into()),
            credentials: None,
        }
    }

    #[test]
    fn registry_replace_upsert_remove() {
        let r = Registry::new_shared();
        r.write()
            .unwrap()
            .replace_all(vec![profile_fixture("a"), profile_fixture("b")]);
        assert_eq!(r.read().unwrap().len(), 2);
        r.write().unwrap().upsert(profile_fixture("a")); // замена
        assert!(r.read().unwrap().get("a").is_some());
        assert!(r.write().unwrap().remove("b"));
        assert!(!r.write().unwrap().remove("b")); // повторное удаление — false
        assert_eq!(r.read().unwrap().ids(), vec!["a".to_string()]);
    }
}
