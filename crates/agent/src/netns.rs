//! Чистая логика сетевых namespace'ов: имена, слоты адресов, имена veth.
//!
//! Здесь нет syscall-обёрток — ими занимается задача 9; этот модуль
//! только вычисляет имена и адреса.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use bottle_core::profile::validate_id;

use crate::error::AgentError;

/// Первый выделяемый индекс слота.
const FIRST_SLOT_IDX: u8 = 1;
/// Последний выделяемый индекс слота.
const LAST_SLOT_IDX: u8 = 250;

/// Возвращает имя network namespace для бутылки `id`.
///
/// `id` обязан пройти [`validate_id`]: имя попадает в пути
/// `/run/netns/...` и `/etc/netns/...`, поэтому это барьер инъекций пути.
pub fn netns_name(id: &str) -> Result<String, AgentError> {
    validate_id(id).map_err(|_| AgentError::InvalidId)?;
    Ok(format!("bottle-{id}"))
}

/// Слот адресов подсети /30 для одной бутылки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Индекс слота (1..=250).
    pub idx: u8,
    /// Адрес peer-интерфейса в хост-namespace: `10.77.idx.1`.
    pub peer: Ipv4Addr,
    /// Адрес интерфейса внутри бутылки: `10.77.idx.2`.
    pub inner: Ipv4Addr,
    /// Адрес подсети (без маски): `10.77.idx.0`.
    pub subnet: Ipv4Addr,
}

impl Slot {
    /// Строит слот с адресами по индексу `idx`.
    pub fn new(idx: u8) -> Self {
        Self {
            idx,
            peer: Ipv4Addr::new(10, 77, idx, 1),
            inner: Ipv4Addr::new(10, 77, idx, 2),
            subnet: Ipv4Addr::new(10, 77, idx, 0),
        }
    }
}

/// Таблица занятости сетевых слотов.
#[derive(Debug, Default)]
pub struct SlotTable {
    /// Занятые индексы слотов.
    used: BTreeSet<u8>,
}

impl SlotTable {
    /// Выделяет первый свободный слот с индексом в 1..=250.
    pub fn alloc(&mut self) -> Result<Slot, AgentError> {
        let idx = (FIRST_SLOT_IDX..=LAST_SLOT_IDX)
            .find(|&idx| !self.used.contains(&idx))
            .ok_or(AgentError::NoFreeSlots)?;
        self.used.insert(idx);
        Ok(Slot::new(idx))
    }

    /// Освобождает слот `idx`; незанятый индекс — no-op
    /// (идемпотентный teardown).
    pub fn free(&mut self, idx: u8) {
        self.used.remove(&idx);
    }

    /// Число занятых слотов.
    pub fn len(&self) -> usize {
        self.used.len()
    }

    /// Свободны ли все слоты.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Возвращает имена пары veth-интерфейсов слота `idx`:
/// (хостовый peer, интерфейс внутри бутылки).
pub fn veth_names(idx: u8) -> (String, String) {
    (format!("p{idx}"), format!("b{idx}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netns_name_rejects_path_traversal() {
        let err = netns_name("../evil").expect_err("path traversal должен быть отклонён");
        assert!(matches!(err, AgentError::InvalidId));
    }

    #[test]
    fn netns_name_rejects_non_ascii() {
        let err = netns_name("офис").expect_err("кириллица должна быть отклонена");
        assert!(matches!(err, AgentError::InvalidId));
    }

    #[test]
    fn netns_name_formats_valid_id() {
        let name = netns_name("office-a").expect("валидный id");
        assert_eq!(name, "bottle-office-a");
    }

    #[test]
    fn alloc_is_monotonic_then_reuses_freed_slot() {
        let mut table = SlotTable::default();
        assert_eq!(table.alloc().expect("слот 1").idx, 1);
        assert_eq!(table.alloc().expect("слот 2").idx, 2);
        assert_eq!(table.alloc().expect("слот 3").idx, 3);

        table.free(2);
        assert_eq!(table.alloc().expect("переиспользованный слот 2").idx, 2);
    }

    #[test]
    fn alloc_after_free_of_last_returns_same_idx() {
        let mut table = SlotTable::default();
        table.alloc().expect("слот 1");
        let last = table.alloc().expect("слот 2");

        table.free(last.idx);
        let again = table.alloc().expect("переиспользованный слот");
        assert_eq!(again.idx, last.idx);
    }

    #[test]
    fn alloc_exhaustion_returns_no_free_slots() {
        let mut table = SlotTable::default();
        for expected in FIRST_SLOT_IDX..=LAST_SLOT_IDX {
            assert_eq!(table.alloc().expect("слот из диапазона").idx, expected);
        }
        let err = table.alloc().expect_err("все слоты заняты");
        assert!(matches!(err, AgentError::NoFreeSlots));
    }

    #[test]
    fn free_of_unallocated_idx_is_noop() {
        let mut table = SlotTable::default();
        table.free(42);
        assert_eq!(table.len(), 0, "free незанятого не должен менять len");
        assert_eq!(table.alloc().expect("первый слот").idx, 1);
    }

    #[test]
    fn len_tracks_alloc_and_free() {
        let mut table = SlotTable::default();
        table.alloc().expect("слот 1");
        table.alloc().expect("слот 2");
        assert_eq!(table.len(), 2);

        table.free(1);
        assert_eq!(table.len(), 1);

        table.free(1); // повторный free — no-op
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn slot_addresses_follow_layout() {
        let slot = Slot::new(3);
        assert_eq!(slot.idx, 3);
        assert_eq!(slot.peer, Ipv4Addr::new(10, 77, 3, 1));
        assert_eq!(slot.inner, Ipv4Addr::new(10, 77, 3, 2));
        assert_eq!(slot.subnet, Ipv4Addr::new(10, 77, 3, 0));
    }

    #[test]
    fn veth_names_pair_for_idx() {
        assert_eq!(veth_names(7), ("p7".to_string(), "b7".to_string()));
    }
}
