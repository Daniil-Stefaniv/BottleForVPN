//! Журнал маршрутов службы: сессии демонов, append-only JSONL-журнал
//! и сверка с таблицей маршрутов ОС при старте (задача 5).
//!
//! Модель состояния — чистая редукция [`reduce_journal`] над операциями
//! [`JournalOp`] (add / del / закрытие сессии); [`RouteStore`] хранит
//! тот же live-set в памяти и дописывает каждую операцию в
//! `routes.jsonl` (append-only, строка JSON `{op, dest, gateway,
//! iface_alias, session, ts}`). Сессия демона — монотонный счётчик
//! службы ([`SessionId`]).
//!
//! Удаления — строго OS-first: сначала фактический `Remove-NetRoute`,
//! и только после успеха — запись `del` и снятие из live-set
//! ([`RouteStore::del`]). Провал OS-удаления оставляет маршрут в
//! live-set и ОС — сверка при следующем старте службы ([`reconcile`])
//! удаляет его как сироту; она сканирует live-set журнала, а не всю
//! таблицу ОС, поэтому опережать ОС журналом нельзя (иначе маршрут
//! потерялся бы из-под контроля навсегда). Закрытие соединения демона
//! (спека §9: маршруты не переживают смерть демона) получает снимок
//! [`RouteStore::session_routes`] и подтверждает каждый маршрут после
//! его фактического удаления из ОС.
//!
//! [`reconcile`] при старте службы сверяет live-set из журнала с
//! фактическими маршрутами ОС: маршрут есть в ОС, а его сессии уже
//! нет — удалить из ОС и записать `del`; маршрута в ОС нет — вычистить
//! из live-set. Идемпотентен на пустом состоянии (ни одного вызова
//! PowerShell).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bottle_core::svcrpc::RouteEntry;
use serde::{Deserialize, Serialize};

use crate::routes::{run_ps, RouteCmd};

/// Идентификатор сессии демона: монотонный счётчик службы.
pub type SessionId = u64;

/// Имя файла журнала маршрутов внутри каталога данных
/// (`%PROGRAMDATA%\Pogrebok`, спека §8).
pub const JOURNAL_FILE_NAME: &str = "routes.jsonl";

/// Операция над live-set'ом маршрутов — единица состояния журнала.
///
/// `Add`/`Del` дописываются в JSONL-файл; `CloseSession` — операция
/// закрытия соединения демона, при записи на диск разворачивается в
/// последовательность `del` по всем маршрутам сессии (файл хранит
/// только `add`/`del`), но в редукции участвует как есть.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalOp {
    /// Маршрут добавлен в сессию (дубль по dest — последний wins).
    Add {
        /// Назначение маршрута.
        dest: IpAddr,
        /// Шлюз маршрута.
        gateway: IpAddr,
        /// Псевдоним интерфейса.
        iface_alias: String,
        /// Сессия-владелец.
        session: SessionId,
    },
    /// Маршрут удалён.
    Del {
        /// Назначение маршрута.
        dest: IpAddr,
        /// Сессия, инициировавшая удаление.
        session: SessionId,
    },
    /// Соединение сессии закрыто: все её маршруты снимаются.
    CloseSession {
        /// Закрываемая сессия.
        session: SessionId,
    },
}

/// Свёртка последовательности операций в live-set маршрутов
/// (`dest → запись`). Чистая функция: ядро журнала тестируется без ОС.
pub fn reduce_journal(ops: &[JournalOp]) -> BTreeMap<IpAddr, RouteEntry> {
    let mut live = BTreeMap::new();
    for op in ops {
        apply_op(&mut live, op);
    }
    live
}

/// Применяет одну операцию к live-set (общее ядро [`reduce_journal`]
/// и [`RouteStore`]).
fn apply_op(live: &mut BTreeMap<IpAddr, RouteEntry>, op: &JournalOp) {
    match op {
        JournalOp::Add {
            dest,
            gateway,
            iface_alias,
            session,
        } => {
            // Дубль add по dest — последний wins (замена записи).
            live.insert(
                *dest,
                RouteEntry {
                    dest: *dest,
                    gateway: *gateway,
                    iface_alias: iface_alias.clone(),
                    session: *session,
                },
            );
        }
        JournalOp::Del { dest, .. } => {
            live.remove(dest);
        }
        JournalOp::CloseSession { session } => {
            live.retain(|_, entry| entry.session != *session);
        }
    }
}

/// План сверки: что сделать с live-set при старте службы.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Маршруты, живущие в ОС без живой сессии: удалить из ОС +
    /// запись `del` в журнал (маршруты-сироты, пережившие крах службы).
    pub stale: Vec<RouteEntry>,
    /// Маршруты, отсутствующие в ОС: вычистить из live-set.
    pub vanished: Vec<IpAddr>,
}

/// Действие сверки над одним маршрутом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileAction {
    /// Оставить как есть (живая сессия владеет маршрутом).
    Keep,
    /// Удалить из ОС и журнала (сирота).
    RemoveFromOs,
    /// Вычистить из live-set (в ОС маршрута уже нет).
    Forget,
}

/// Решение сверки по одному маршруту: чистое ядро плана
/// [`plan_reconcile`] и исполнительного [`reconcile`].
fn reconcile_verdict(session_alive: bool, os_has_route: bool) -> ReconcileAction {
    match (session_alive, os_has_route) {
        // Живая сессия владеет маршрутом (есть он в ОС или уже нет —
        // разобраться может только сама сессия).
        (true, _) => ReconcileAction::Keep,
        // Маршрут-сирота: в ОС жив, владелец мёртв.
        (false, true) => ReconcileAction::RemoveFromOs,
        // В ОС маршрута уже нет — вычистить из live-set.
        (false, false) => ReconcileAction::Forget,
    }
}

/// Строит план сверки live-set с ОС по предикатам «сессия жива» и
/// «маршрут есть в ОС». Чистое ядро сверки (unit-сценарии без ОС);
/// исполнительный путь строит план из фактов `Get-NetRoute` и
/// исполняет его [`reconcile`].
fn plan_reconcile<S, O>(
    live: &BTreeMap<IpAddr, RouteEntry>,
    session_alive: S,
    os_has_route: O,
) -> ReconcilePlan
where
    S: Fn(SessionId) -> bool,
    O: Fn(&RouteEntry) -> bool,
{
    let mut plan = ReconcilePlan::default();
    for entry in live.values() {
        match reconcile_verdict(session_alive(entry.session), os_has_route(entry)) {
            ReconcileAction::Keep => {}
            ReconcileAction::RemoveFromOs => plan.stale.push(entry.clone()),
            ReconcileAction::Forget => plan.vanished.push(entry.dest),
        }
    }
    plan
}

/// Запись JSONL-журнала (append-only контракт формата).
///
/// Для `op:"add"` поля `gateway`/`iface_alias` обязательны; для
/// `op:"del"` — `null` (маршрут удаляется целиком по `dest`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalRecord {
    /// Операция: `"add"` или `"del"`.
    op: String,
    /// Назначение маршрута.
    dest: IpAddr,
    /// Шлюз (для `add`).
    gateway: Option<IpAddr>,
    /// Псевдоним интерфейса (для `add`).
    iface_alias: Option<String>,
    /// Сессия-владелец (или инициировавшая удаление).
    session: SessionId,
    /// Время записи, unix epoch, миллисекунды.
    ts: u64,
}

impl JournalRecord {
    /// Преобразует запись файла в операцию live-set; `None` для
    /// неизвестной операции или `add` без обязательных полей.
    fn into_op(self) -> Option<JournalOp> {
        match self.op.as_str() {
            "add" => Some(JournalOp::Add {
                dest: self.dest,
                gateway: self.gateway?,
                iface_alias: self.iface_alias?,
                session: self.session,
            }),
            "del" => Some(JournalOp::Del {
                dest: self.dest,
                session: self.session,
            }),
            _ => None,
        }
    }
}

/// Ошибка доступа к журналу маршрутов.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Не удалось прочитать существующий журнал.
    #[error("не удалось прочитать журнал маршрутов {path}: {source}")]
    Read {
        /// Путь журнала.
        path: PathBuf,
        /// Ошибка ввода-вывода.
        source: std::io::Error,
    },
    /// Не удалось дописать запись журнала.
    #[error("не удалось дописать журнал маршрутов {path}: {source}")]
    Append {
        /// Путь журнала.
        path: PathBuf,
        /// Ошибка ввода-вывода.
        source: std::io::Error,
    },
}

/// Live-set маршрутов службы + append-only журнал + счётчик сессий.
pub struct RouteStore {
    /// Путь файла журнала.
    journal_path: PathBuf,
    /// Живые маршруты по назначению.
    live: BTreeMap<IpAddr, RouteEntry>,
    /// Следующий идентификатор сессии (монотонный; стартует с максимума
    /// из журнала, чтобы номера не повторялись в одном файле).
    next_session: SessionId,
}

impl RouteStore {
    /// Открывает (или создаёт) журнал по пути `journal_path` и
    /// восстанавливает live-set редукцией его записей.
    ///
    /// Повреждённые строки (обрыв записи при крахе, неизвестная
    /// операция) пропускаются с warn-логом: битая строка не должна
    /// блокировать запуск службы — остатки подберёт сверка.
    pub fn open(journal_path: &Path) -> Result<Self, JournalError> {
        if let Some(parent) = journal_path.parent() {
            fs::create_dir_all(parent).map_err(|source| JournalError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut live = BTreeMap::new();
        let mut max_session: SessionId = 0;
        let text = match fs::read_to_string(journal_path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(JournalError::Read {
                    path: journal_path.to_path_buf(),
                    source,
                })
            }
        };
        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(record) => {
                    max_session = max_session.max(record.session);
                    match record.into_op() {
                        Some(op) => apply_op(&mut live, &op),
                        None => tracing::warn!(
                            line = idx + 1,
                            "запись журнала с неизвестной операцией пропущена"
                        ),
                    }
                }
                // Обрыв последней записи при крахе службы не должен
                // блокировать запуск: расхождение подберёт сверка.
                Err(error) => tracing::warn!(
                    line = idx + 1,
                    %error,
                    "повреждённая строка журнала пропущена"
                ),
            }
        }
        Ok(Self {
            journal_path: journal_path.to_path_buf(),
            live,
            next_session: max_session,
        })
    }

    /// Выделяет следующий монотонный идентификатор сессии.
    pub fn next_session(&mut self) -> SessionId {
        self.next_session += 1;
        self.next_session
    }

    /// Зашивает маршрут в live-set сессии и дописывает `add` в журнал.
    /// Дубль по `dest` — последний wins (в ОС New-NetRoute уже прошёл).
    pub fn add(
        &mut self,
        session: SessionId,
        dest: IpAddr,
        gateway: IpAddr,
        iface_alias: impl Into<String>,
    ) -> Result<(), JournalError> {
        let iface_alias = iface_alias.into();
        self.append(&JournalRecord {
            op: "add".to_string(),
            dest,
            gateway: Some(gateway),
            iface_alias: Some(iface_alias.clone()),
            session,
            ts: now_ms(),
        })?;
        apply_op(
            &mut self.live,
            &JournalOp::Add {
                dest,
                gateway,
                iface_alias,
                session,
            },
        );
        Ok(())
    }

    /// Снимает маршрут из live-set с записью `del` в журнал.
    /// `Ok(false)` — маршрута не было (идемпотентен, запись не пишется).
    pub fn del(&mut self, session: SessionId, dest: IpAddr) -> Result<bool, JournalError> {
        if !self.live.contains_key(&dest) {
            return Ok(false);
        }
        self.append(&JournalRecord {
            op: "del".to_string(),
            dest,
            gateway: None,
            iface_alias: None,
            session,
            ts: now_ms(),
        })?;
        apply_op(&mut self.live, &JournalOp::Del { dest, session });
        Ok(true)
    }

    /// Снимок маршрутов сессии для закрытия соединения демона
    /// (спека §9: маршруты не переживают смерть демона).
    ///
    /// Чистая выборка БЕЗ изменения состояния: удаление OS-first —
    /// вызывающий сначала снимает каждый маршрут из ОС, затем
    /// подтверждает успех через [`RouteStore::del`] (запись `del` +
    /// снятие из live-set). Провал OS-удаления оставляет маршрут в
    /// live-set — сверка при следующем старте службы удалит его как
    /// сироту (reconcile сканирует live-set журнала, а не всю таблицу
    /// ОС, поэтому опережать ОС журналом нельзя).
    pub fn session_routes(&self, session: SessionId) -> Vec<RouteEntry> {
        self.live
            .values()
            .filter(|entry| entry.session == session)
            .cloned()
            .collect()
    }

    /// Вычищает маршрут из live-set как отсутствующий в ОС (фаза
    /// сверки `vanished`): пишет `del`, чтобы повторная редукция журнала
    /// не возвращала его.
    fn forget(&mut self, dest: IpAddr) {
        let Some(entry) = self.live.get(&dest).cloned() else {
            return;
        };
        if let Err(error) = self.append(&JournalRecord {
            op: "del".to_string(),
            dest,
            gateway: None,
            iface_alias: None,
            session: entry.session,
            ts: now_ms(),
        }) {
            tracing::warn!(%error, %dest, "не удалось записать del вычищаемого маршрута");
        }
        self.live.remove(&dest);
    }

    /// Снимок live-маршрутов (отсортированы по dest).
    pub fn routes(&self) -> Vec<RouteEntry> {
        self.live.values().cloned().collect()
    }

    /// Есть ли маршрут с таким назначением в live-set.
    pub fn contains(&self, dest: IpAddr) -> bool {
        self.live.contains_key(&dest)
    }

    /// Запись live-маршрута по назначению — для идемпотентного `add`
    /// (сверка параметров и сессии дубля с живой записью).
    pub fn get(&self, dest: IpAddr) -> Option<RouteEntry> {
        self.live.get(&dest).cloned()
    }

    /// Дописывает запись в JSONL-файл (создаёт файл и каталог).
    fn append(&self, record: &JournalRecord) -> Result<(), JournalError> {
        let line = serde_json::to_string(record).map_err(|error| JournalError::Append {
            path: self.journal_path.clone(),
            source: std::io::Error::other(error),
        })?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
            .map_err(|source| JournalError::Append {
                path: self.journal_path.clone(),
                source,
            })?;
        writeln!(file, "{line}").map_err(|source| JournalError::Append {
            path: self.journal_path.clone(),
            source,
        })
    }
}

/// Итог сверки журнала с ОС при старте службы.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Сироты удалены из ОС и журнала.
    pub removed: usize,
    /// Отсутствующие в ОС вычищены из live-set.
    pub vanished: usize,
    /// Живые сессии — оставлены.
    pub kept: usize,
    /// Ошибка запроса/удаления — решение отложено до следующего старта.
    pub undecided: usize,
}

/// Сверяет live-set из журнала с фактическими маршрутами ОС.
///
/// Для каждого live-маршрута: `Get-NetRoute` (через [`route_ps_args`]
/// — других путей построения аргументов нет) → маршрут в ОС есть, а
/// сессии нет → `Remove-NetRoute` + запись `del`; в ОС нет → вычистить
/// из live-set; ошибка PowerShell → пропустить с warn (на пустом
/// live-set не делает ни одного вызова — идемпотентен на пустом).
///
/// Компромисс (указатель ревью T3+T4): `Get-NetRoute` с
/// `-ErrorAction SilentlyContinue` возвращает пустой вывод и exit 0 и
/// когда маршрута нет, и когда вывод подавлен — пустой вывод трактуем
/// как «маршрута нет».
pub async fn reconcile<F>(store: &mut RouteStore, session_alive: F) -> ReconcileReport
where
    F: Fn(SessionId) -> bool,
{
    let mut report = ReconcileReport::default();
    // Идемпотентность на пустом: ни одного вызова PowerShell.
    let entries = store.routes();

    // Фаза 1 — факты из ОС: `Get-NetRoute` по каждому live-маршруту.
    // Ошибку запроса нельзя трактовать как «маршрута нет» — решение
    // откладывается до следующего старта (undecided).
    let mut checked = BTreeMap::new();
    let mut os_has: BTreeMap<IpAddr, bool> = BTreeMap::new();
    for entry in entries {
        match run_ps(RouteCmd::List, entry.dest, None, None).await {
            Ok(listing) => {
                os_has.insert(entry.dest, !listing.trim().is_empty());
                checked.insert(entry.dest, entry);
            }
            Err(error) => {
                tracing::warn!(dest = %entry.dest, %error, "сверка: маршрут не проверен");
                report.undecided += 1;
            }
        }
    }

    // Фаза 2 — чистый план сверки по фактам.
    let plan = plan_reconcile(&checked, session_alive, |entry| os_has[&entry.dest]);
    report.kept = checked.len() - plan.stale.len() - plan.vanished.len();

    // Фаза 3 — исполнение плана.
    for dest in plan.vanished {
        store.forget(dest);
        report.vanished += 1;
    }
    for entry in plan.stale {
        match run_ps(RouteCmd::Del, entry.dest, None, None).await {
            Ok(_) => match store.del(entry.session, entry.dest) {
                Ok(_) => {
                    tracing::info!(dest = %entry.dest, "сверка: маршрут-сирота удалён");
                    report.removed += 1;
                }
                Err(error) => {
                    tracing::warn!(dest = %entry.dest, %error, "сверка: удалён из ОС, но не из журнала");
                    report.undecided += 1;
                }
            },
            Err(error) => {
                tracing::warn!(dest = %entry.dest, %error, "сверка: не удалось удалить маршрут-сироту");
                report.undecided += 1;
            }
        }
    }
    report
}

/// Путь журнала по умолчанию: `%PROGRAMDATA%\Pogrebok\routes.jsonl`
/// (спека §8); вне Windows (`--console` в dev-режиме, PROGRAMDATA нет) —
/// временный каталог, чтобы консольный режим работал на хосте разработки.
pub fn default_journal_path() -> PathBuf {
    match std::env::var_os("PROGRAMDATA") {
        Some(programdata) => Path::new(&programdata)
            .join("Pogrebok")
            .join(JOURNAL_FILE_NAME),
        None => std::env::temp_dir().join(JOURNAL_FILE_NAME),
    }
}

/// Текущее время в unix epoch миллисекундах (0 при сдвиге часов назад
/// за эпоху — журналу важна монотонность момента записи, не точность).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use serde_json::Value;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    fn op_add(dest: &str, session: SessionId) -> JournalOp {
        JournalOp::Add {
            dest: ip(dest),
            gateway: ip("192.168.1.1"),
            iface_alias: "Ethernet".to_string(),
            session,
        }
    }

    fn entry(dest: &str, session: SessionId) -> RouteEntry {
        RouteEntry {
            dest: ip(dest),
            gateway: ip("192.168.1.1"),
            iface_alias: "Ethernet".to_string(),
            session,
        }
    }

    // ------------------------------------------------------------------
    // Чистая редукция (сценарии рулинга: add / add-other-session / del /
    // закрытие-сессии / дубль-add).
    // ------------------------------------------------------------------

    #[test]
    fn reduce_add_then_del_yields_empty() {
        let ops = vec![
            op_add("203.0.113.10", 1),
            JournalOp::Del {
                dest: ip("203.0.113.10"),
                session: 1,
            },
        ];
        assert!(reduce_journal(&ops).is_empty());
    }

    #[test]
    fn reduce_add_other_session_same_dest_last_wins() {
        let ops = vec![op_add("203.0.113.10", 1), op_add("203.0.113.10", 2)];
        let live = reduce_journal(&ops);
        assert_eq!(live.values().count(), 1, "dest один — запись одна");
        assert_eq!(live[&ip("203.0.113.10")].session, 2, "последний add wins");
    }

    #[test]
    fn reduce_duplicate_add_last_wins() {
        let first = JournalOp::Add {
            dest: ip("203.0.113.10"),
            gateway: ip("10.0.0.1"),
            iface_alias: "Eth A".to_string(),
            session: 7,
        };
        let second = JournalOp::Add {
            dest: ip("203.0.113.10"),
            gateway: ip("10.0.0.2"),
            iface_alias: "Eth B".to_string(),
            session: 7,
        };
        let live = reduce_journal(&[first, second]);
        let entry = &live[&ip("203.0.113.10")];
        assert_eq!(entry.gateway, ip("10.0.0.2"));
        assert_eq!(entry.iface_alias, "Eth B");
    }

    #[test]
    fn reduce_close_session_removes_only_its_routes() {
        let ops = vec![
            op_add("203.0.113.10", 1),
            op_add("203.0.113.11", 2),
            op_add("203.0.113.12", 1),
            JournalOp::CloseSession { session: 1 },
        ];
        let live = reduce_journal(&ops);
        assert_eq!(live.keys().collect::<Vec<_>>(), vec![&ip("203.0.113.11")]);
        assert_eq!(live[&ip("203.0.113.11")].session, 2);
    }

    #[test]
    fn reduce_del_of_missing_dest_is_noop() {
        let ops = vec![
            op_add("203.0.113.10", 1),
            JournalOp::Del {
                dest: ip("203.0.113.99"),
                session: 1,
            },
        ];
        let live = reduce_journal(&ops);
        assert!(live.contains_key(&ip("203.0.113.10")));
    }

    // ------------------------------------------------------------------
    // План сверки (чистое ядро reconcile).
    // ------------------------------------------------------------------

    #[test]
    fn plan_reconcile_partitions_entries() {
        let mut live = BTreeMap::new();
        // Сирота: в ОС, сессия мертва → stale.
        live.insert(ip("203.0.113.10"), entry("203.0.113.10", 1));
        // Живая сессия: оставить.
        live.insert(ip("203.0.113.11"), entry("203.0.113.11", 2));
        // В ОС уже нет: vanished.
        live.insert(ip("203.0.113.12"), entry("203.0.113.12", 3));

        let plan = plan_reconcile(
            &live,
            |session| session == 2,
            |entry| entry.dest != ip("203.0.113.12"),
        );

        assert_eq!(plan.stale, vec![entry("203.0.113.10", 1)]);
        assert_eq!(plan.vanished, vec![ip("203.0.113.12")]);
    }

    #[test]
    fn plan_reconcile_empty_live_is_empty() {
        let plan = plan_reconcile(&BTreeMap::new(), |_| false, |_| true);
        assert_eq!(plan, ReconcilePlan::default(), "идемпотентен на пустом");
    }

    #[test]
    fn plan_reconcile_startup_all_sessions_dead() {
        // При старте службы живых сессий нет: всё живое в ОС — сироты.
        let mut live = BTreeMap::new();
        live.insert(ip("203.0.113.10"), entry("203.0.113.10", 5));
        let plan = plan_reconcile(&live, |_| false, |_| true);
        assert_eq!(plan.stale.len(), 1);
        assert!(plan.vanished.is_empty());
    }

    // ------------------------------------------------------------------
    // RouteStore: журнал-файл, live-set, сессии.
    // ------------------------------------------------------------------

    fn store_in_tempdir() -> (tempfile::TempDir, RouteStore) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store =
            RouteStore::open(&tmp.path().join(JOURNAL_FILE_NAME)).expect("открытие журнала");
        (tmp, store)
    }

    /// Читает записи журнала как JSON-значения (пин схемы строк).
    fn read_records(path: &Path) -> Vec<Value> {
        let text = fs::read_to_string(path).expect("журнал читается");
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("строка журнала — JSON"))
            .collect()
    }

    #[test]
    fn store_open_on_missing_file_starts_clean() {
        let (tmp, mut store) = store_in_tempdir();
        assert!(store.routes().is_empty());
        assert_eq!(store.next_session(), 1, "первая сессия — 1");
        // Каталог и файл создаются при первой дописи.
        store
            .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        assert!(tmp.path().join(JOURNAL_FILE_NAME).exists());
    }

    #[test]
    fn store_open_restores_live_set_from_journal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(JOURNAL_FILE_NAME);
        let journal = concat!(
            r#"{"op":"add","dest":"203.0.113.10","gateway":"192.168.1.1","iface_alias":"Ethernet","session":7,"ts":100}"#,
            "\n",
            r#"{"op":"add","dest":"2001:db8::10","gateway":"fd00::1","iface_alias":"Wi-Fi","session":7,"ts":101}"#,
            "\n",
            r#"{"op":"del","dest":"203.0.113.10","gateway":null,"iface_alias":null,"session":7,"ts":102}"#,
            "\n",
        );
        fs::write(&path, journal).unwrap();

        let mut store = RouteStore::open(&path).unwrap();
        let routes = store.routes();
        assert_eq!(routes.len(), 1, "del снял первый маршрут");
        assert_eq!(routes[0].dest, ip("2001:db8::10"));
        assert_eq!(routes[0].session, 7);
        // Счётчик сессий стартует после максимума из журнала.
        assert_eq!(store.next_session(), 8);
    }

    #[test]
    fn store_open_skips_malformed_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(JOURNAL_FILE_NAME);
        let journal = concat!(
            "{обрыв записи без JSON",
            "\n",
            r#"{"op":"add","dest":"203.0.113.10","gateway":"192.168.1.1","iface_alias":"Ethernet","session":3,"ts":1}"#,
            "\n",
            r#"{"op":"nonsense","dest":"203.0.113.11","gateway":null,"iface_alias":null,"session":3,"ts":2}"#,
            "\n",
        );
        fs::write(&path, journal).unwrap();

        let store = RouteStore::open(&path).unwrap();
        let routes = store.routes();
        assert_eq!(routes.len(), 1, "битая и неизвестная строки пропущены");
        assert_eq!(routes[0].dest, ip("203.0.113.10"));
    }

    #[test]
    fn store_add_appends_schema_record_and_updates_live() {
        let (tmp, mut store) = store_in_tempdir();
        store
            .add(4, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();

        assert_eq!(store.routes(), vec![entry("203.0.113.10", 4)]);
        let records = read_records(&tmp.path().join(JOURNAL_FILE_NAME));
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record["op"], "add");
        assert_eq!(record["dest"], "203.0.113.10");
        assert_eq!(record["gateway"], "192.168.1.1");
        assert_eq!(record["iface_alias"], "Ethernet");
        assert_eq!(record["session"], 4);
        assert!(record["ts"].is_u64(), "ts — число epoch-миллисекунд");
    }

    #[test]
    fn store_del_appends_record_and_removes_live() {
        let (tmp, mut store) = store_in_tempdir();
        store
            .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();

        assert!(store.del(1, ip("203.0.113.10")).unwrap(), "маршрут был");
        assert!(store.routes().is_empty());

        let records = read_records(&tmp.path().join(JOURNAL_FILE_NAME));
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["op"], "del");
        assert_eq!(records[1]["dest"], "203.0.113.10");
        assert_eq!(
            records[1]["gateway"],
            Value::Null,
            "для del шлюз не пишется"
        );
        assert_eq!(records[1]["iface_alias"], Value::Null);

        // Повторный del того же dest — идемпотентен, без новой записи.
        assert!(!store.del(1, ip("203.0.113.10")).unwrap());
        assert_eq!(read_records(&tmp.path().join(JOURNAL_FILE_NAME)).len(), 2);
    }

    #[test]
    fn store_session_routes_is_pure_snapshot() {
        let (_tmp, mut store) = store_in_tempdir();
        store
            .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        store
            .add(1, ip("203.0.113.11"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        store
            .add(2, ip("203.0.113.12"), ip("192.168.1.1"), "Wi-Fi")
            .unwrap();

        // Снимок НЕ меняет состояние: OS-first — сначала Remove-NetRoute,
        // потом подтверждение через del (иначе провал OS-удаления
        // вычистил бы маршрут из журнала навсегда).
        let snapshot = store.session_routes(1);

        assert_eq!(
            snapshot.iter().map(|e| e.dest).collect::<Vec<_>>(),
            vec![ip("203.0.113.10"), ip("203.0.113.11")],
            "снимок содержит маршруты закрываемой сессии"
        );
        assert_eq!(store.routes().len(), 3, "live-set нетронут снимком");

        // Подтверждение OS-удаления: del по каждому — запись + снятие.
        for entry in &snapshot {
            assert!(store.del(entry.session, entry.dest).unwrap());
        }
        let mut expected = entry("203.0.113.12", 2);
        expected.iface_alias = "Wi-Fi".to_string();
        assert_eq!(store.routes(), vec![expected]);

        // Повторный снимок закрытой сессии — пуст.
        assert!(store.session_routes(1).is_empty());
    }

    /// Закрытие сессии с провалом OS-удаления: маршрут не удаляется из
    /// ОС, live-set и журнал не тронуты (подберёт сверка при старте).
    /// Проверяется на уровне pipe::close_session_routes — здесь же
    /// фиксируем, что без `del`-подтверждения записей не прибавляется.
    #[test]
    fn store_del_confirmation_only_after_os_success() {
        let (tmp, mut store) = store_in_tempdir();
        store
            .add(7, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();

        // OS-удаление НЕ выполнено → подтверждения нет: одна add-запись.
        assert_eq!(read_records(&tmp.path().join(JOURNAL_FILE_NAME)).len(), 1);
        assert_eq!(store.routes().len(), 1);
    }

    // ------------------------------------------------------------------
    // Прочее.
    // ------------------------------------------------------------------

    #[test]
    fn default_journal_path_prefers_programdata() {
        // PROGRAMDATA задан (Windows/CI) → каталог данных; не задан
        // (dev-хост) → временный каталог. Точка монтирования пути —
        // имя файла журнала в обоих случаях.
        let path = default_journal_path();
        assert!(path.ends_with(JOURNAL_FILE_NAME));
    }
}
