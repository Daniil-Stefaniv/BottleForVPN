//! Сетевые namespace'ы бутылок: чистая логика (имена, слоты, veth)
//! и syscall-слой жизненного цикла (задача 9).
//!
//! Syscall-слой управляет системой через `ip` из iproute2 и работает
//! только под root внутри аплаенса (Linux); на других платформах код
//! компилируется, но операции завершаются ошибкой.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use bottle_core::profile::validate_id;
use bottle_core::protocol::ForwardPort;
use relay_lib::forward::{serve_forward, TargetAddr};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

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

/// Корень per-namespace конфигурации iproute2 (bind-mount поверх `/etc`).
const NETNS_ETC_ROOT: &str = "/etc/netns";
/// resolv.conf VM (default namespace) — источник DNS «через VM».
const VM_RESOLV_CONF: &str = "/etc/resolv.conf";
/// Переменная окружения, переопределяющая исполняемый файл relay.
///
/// Интеграционные тесты подменяют её на собранный `bottle-agent`:
/// `current_exe()` тестового бинарника — это harness libtest, а не агент.
const RELAY_EXE_ENV: &str = "BOTTLE_AGENT_RELAY_EXE";

/// Ручка поднятой бутылки: все ресурсы её network namespace.
#[derive(Debug)]
pub struct NetnsHandle {
    /// Идентификатор бутылки.
    pub id: String,
    /// Имя network namespace (`bottle-<id>`).
    pub ns_name: String,
    /// Слот адресов подсети `/30`.
    pub slot: Slot,
    /// Процесс relay внутри namespace (после `execvp` это сам relay,
    /// а не `ip`: `ip netns exec` выполняет команду без fork).
    pub relay_child: Child,
    /// Процесс wireguard-go туннеля внутри namespace
    /// (`wg::tunnel_up_wg`/`wg::tunnel_down`, задача 10);
    /// `None`, пока туннель не поднят.
    pub tunnel_child: Option<Child>,
    /// Задачи мостов в default namespace (SOCKS5 + пробросы).
    pub bridge_tasks: JoinSet<()>,
}

/// Поднимает network namespace бутылки `id` на слоте `slot`.
///
/// Порядок: (1) мосты в default namespace занимают порты до каких-либо
/// изменений системы — занятый порт отвергается без созданных ресурсов;
/// (2) `ip netns add` + veth-пара + адреса + маршруты; (3) запуск relay
/// внутри namespace (`ip netns exec`). При любой неудаче частично
/// поднятые ресурсы сносятся, а наружу возвращается исходная ошибка.
///
/// Мост — тупое копирование байтов: SOCKS5 и пробросы терминируются
/// relay внутри namespace, поэтому протокол полностью изолирован
/// в бутылке.
pub async fn netns_up(
    slot: &Slot,
    id: &str,
    listeners: &[ForwardPort],
    socks5_port: u16,
) -> Result<NetnsHandle, AgentError> {
    let ns_name = netns_name(id)?;
    let (peer_veth, inner_veth) = veth_names(slot.idx);
    let mut bridge_tasks = JoinSet::new();

    // Первый fallible-этап: при отказе любого бинда уже поднятые мосты
    // останавливаются самой bind_bridges — ранний `?` здесь безопасен.
    bind_bridges(&mut bridge_tasks, socks5_port, listeners, slot.inner).await?;

    let mut ns_created = false;
    let outcome: Result<Child, AgentError> = async {
        run_cmd("ip", &["netns", "add", &ns_name])?;
        ns_created = true;

        add_veth_pair(&peer_veth, &inner_veth)?;
        run_cmd("ip", &["link", "set", &inner_veth, "netns", &ns_name])?;
        run_cmd(
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/30", slot.peer),
                "dev",
                &peer_veth,
            ],
        )?;
        run_cmd("ip", &["link", "set", &peer_veth, "up"])?;
        run_in_ns(
            &ns_name,
            &[
                "ip",
                "addr",
                "add",
                &format!("{}/30", slot.inner),
                "dev",
                &inner_veth,
            ],
        )?;
        run_in_ns(&ns_name, &["ip", "link", "set", &inner_veth, "up"])?;
        run_in_ns(&ns_name, &["ip", "link", "set", "lo", "up"])?;
        run_in_ns(
            &ns_name,
            &[
                "ip",
                "route",
                "add",
                "default",
                "via",
                &slot.peer.to_string(),
            ],
        )?;

        // Последний fallible-шаг: после него ручка строится без отказов.
        spawn_relay(&ns_name, socks5_port, listeners)
    }
    .await;

    match outcome {
        Ok(relay_child) => Ok(NetnsHandle {
            id: id.to_string(),
            ns_name,
            slot: *slot,
            relay_child,
            tunnel_child: None,
            bridge_tasks,
        }),
        Err(error) => {
            // Исходная ошибка важнее отказов очистки.
            bridge_tasks.abort_all();
            if ns_created {
                if let Err(teardown) = run_cmd("ip", &["netns", "del", &ns_name]) {
                    tracing::warn!(
                        %teardown,
                        "не удалось удалить частично поднятый namespace {ns_name}"
                    );
                }
            }
            remove_netns_etc(&ns_name);
            Err(error)
        }
    }
}

/// Биндит все мосты бутылки (SOCKS5 + пробросы) в default namespace.
///
/// При отказе любого бинда уже поднятые мосты останавливаются:
/// abort + выкачивание `join_next` гарантируют, что слушатели закрыты
/// и порты свободны к моменту возврата ошибки. Drop `JoinSet` лишь
/// отцепляет задачи — без явной остановки уже занятый мост продолжал
/// бы слушать в default namespace (перенаправляя в несуществующий
/// namespace), теряя порт до перезапуска агента и ломая повторный Up
/// ошибкой AddrInUse. Наружу возвращается исходная ошибка бинда.
async fn bind_bridges(
    tasks: &mut JoinSet<()>,
    socks5_port: u16,
    listeners: &[ForwardPort],
    inner: Ipv4Addr,
) -> Result<(), AgentError> {
    let outcome: Result<(), AgentError> = async {
        spawn_bridge(tasks, socks5_port, inner).await?;
        for listener in listeners {
            spawn_bridge(tasks, listener.port, inner).await?;
        }
        Ok(())
    }
    .await;

    if outcome.is_err() {
        tasks.abort_all();
        // Отменённая задача отпускает слушатель только когда рантайм
        // сбросит её будущее — выкачиваем до последней задачи.
        while tasks.join_next().await.is_some() {}
    }
    outcome
}

/// Опускает бутылку: останавливает relay и мосты, удаляет namespace.
///
/// `ip netns del` уничтожает veth-пару автоматически. Ошибки отдельных
/// шагов логируются и не прерывают разбор; ошибкой возвращается только
/// неудача удаления namespace — она означает утечку имени в `/run/netns`.
pub fn netns_down(handle: NetnsHandle) -> Result<(), AgentError> {
    let NetnsHandle {
        ns_name,
        slot,
        mut relay_child,
        tunnel_child,
        mut bridge_tasks,
        ..
    } = handle;

    if let Err(error) = relay_child.kill() {
        tracing::warn!(%error, "не удалось остановить relay-процесс {ns_name}");
    }
    let _ = relay_child.wait();

    // Туннель обязан быть остановлен `wg::tunnel_down` раньше; здесь —
    // защита от утечки: живой процесс удерживал бы namespace после
    // удаления имени из /run/netns.
    if let Some(mut tunnel_child) = tunnel_child {
        if let Err(error) = tunnel_child.kill() {
            tracing::warn!(%error, "не удалось остановить wireguard-процесс {ns_name}");
        }
        let _ = tunnel_child.wait();
    }

    bridge_tasks.abort_all();

    run_cmd("ip", &["netns", "del", &ns_name])?;
    remove_netns_etc(&ns_name);
    let (host_veth, _) = veth_names(slot.idx);
    wait_host_veth_gone(&host_veth);
    Ok(())
}

/// Создаёт veth-пару слота. Ядро сносит namespace асинхронно: хостовый
/// конец прошлой бутылки на этом слоте может пережить `ip netns del` на
/// миллисекунды (wait_host_veth_gone в netns_down сужает окно, но не
/// гарантирует). «File exists» → снос сироты и одна повторная попытка.
fn add_veth_pair(peer_veth: &str, inner_veth: &str) -> Result<(), AgentError> {
    let args = [
        "link", "add", peer_veth, "type", "veth", "peer", "name", inner_veth,
    ];
    match run_cmd("ip", &args) {
        Ok(_) => Ok(()),
        Err(AgentError::CommandFailed { stderr, .. }) if stderr.contains("File exists") => {
            let _ = run_cmd("ip", &["link", "del", peer_veth]);
            run_cmd("ip", &args).map(|_| ())
        }
        Err(error) => Err(error),
    }
}

/// Ждёт исчезновения хостового конца veth-пары после `ip netns del`.
/// Namespace сносится ядром асинхронно: сокеты отменённых мостов могут
/// недолго удерживать его, и немедленный повторный up того же слота
/// ловил бы «RTNETLINK answers: File exists». Молчаливый предел 3 с —
/// дольше ждать бессмысленно, up сам решит по факту.
fn wait_host_veth_gone(name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let gone = Command::new("ip")
            .args(["link", "show", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| !status.success())
            .unwrap_or(true);
        if gone || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Пишет `/etc/netns/bottle-<id>/resolv.conf` — DNS для namespace.
///
/// `Some(ip)` фиксирует сервер `ip`; `None` копирует текущий
/// `/etc/resolv.conf` VM (default namespace), чтобы DNS внутри бутылки
/// работал наружу через VM, а не в никуда. Каталог создаётся с правами
/// 0755, файл — 0644.
///
/// Файл применяется `ip netns exec` при запуске процесса (bind-mount
/// поверх `/etc/resolv.conf`), поэтому вызывать нужно до запуска
/// процессов в namespace.
pub fn set_dns(id: &str, dns: Option<IpAddr>) -> Result<(), AgentError> {
    let ns_name = netns_name(id)?;
    let dir = netns_etc_dir(&ns_name);
    fs::create_dir_all(&dir)?;
    set_mode(&dir, 0o755)?;

    let content = match dns {
        Some(ip) => format!("nameserver {ip}\n"),
        None => fs::read_to_string(VM_RESOLV_CONF)?,
    };
    let resolv_conf = dir.join("resolv.conf");
    fs::write(&resolv_conf, content)?;
    set_mode(&resolv_conf, 0o644)?;
    Ok(())
}

/// Поднимает мост: слушатель `0.0.0.0:port` в default namespace
/// переправляет соединения на `inner:port` внутри namespace бутылки.
async fn spawn_bridge(
    tasks: &mut JoinSet<()>,
    port: u16,
    inner: Ipv4Addr,
) -> Result<(), AgentError> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let target = TargetAddr::Ip(SocketAddr::V4(SocketAddrV4::new(inner, port)));
    tasks.spawn(async move {
        if let Err(error) = serve_forward(listener, target).await {
            tracing::warn!(port, %error, "мост остановлен");
        }
    });
    Ok(())
}

/// Запускает relay внутри namespace:
/// `ip netns exec <ns> <exe> relay --socks5-port <p> [--forward <p:t>]...`.
///
/// Путь к своему исполняемому файлу разрешается ДО запуска: магическая
/// ссылка `/proc/self/exe`, переданная в `ip netns exec` как команда,
/// разрешится в сам бинарник `ip` (iproute2 выполняет команду обычным
/// `execvp` без специальной обработки), а не в агента. `ip netns exec`
/// не порождает промежуточный процесс, поэтому `kill()` ребёнка в
/// [`netns_down`] завершает именно relay, без осиротевших внуков.
fn spawn_relay(
    ns_name: &str,
    socks5_port: u16,
    listeners: &[ForwardPort],
) -> Result<Child, AgentError> {
    let exe = relay_exe_path()?;
    let mut args = vec![
        "relay".to_string(),
        "--socks5-port".to_string(),
        socks5_port.to_string(),
    ];
    for listener in listeners {
        args.push("--forward".to_string());
        args.push(format!("{}:{}", listener.port, listener.target));
    }

    Command::new("ip")
        .args(["netns", "exec", ns_name])
        .arg(&exe)
        .args(&args)
        // Лог relay недоступен родителю (tracing не наследуется),
        // поэтому вывод направляется прямо в терминал/джорнал аплаенса.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(AgentError::from)
}

/// Возвращает путь к исполняемому файлу relay: явная переменная
/// окружения (тесты) или текущий исполняемый файл (на Linux это и есть
/// чтение `/proc/self/exe` процессом агента).
fn relay_exe_path() -> Result<PathBuf, AgentError> {
    match std::env::var_os(RELAY_EXE_ENV) {
        Some(path) => Ok(PathBuf::from(path)),
        None => std::env::current_exe().map_err(AgentError::from),
    }
}

/// Выполняет команду, захватывая вывод; неудача — [`AgentError::CommandFailed`]
/// с командой и stderr в сообщении.
pub(crate) fn run_cmd(program: &str, args: &[&str]) -> Result<String, AgentError> {
    let output = Command::new(program).args(args).output().map_err(|e| {
        AgentError::Io(io::Error::new(
            e.kind(),
            format!("не удалось запустить {program}: {e}"),
        ))
    })?;
    if !output.status.success() {
        return Err(AgentError::CommandFailed {
            command: format!("{program} {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Выполняет команду внутри namespace через `ip netns exec`.
pub(crate) fn run_in_ns(ns_name: &str, args: &[&str]) -> Result<String, AgentError> {
    let mut argv = vec!["netns", "exec", ns_name];
    argv.extend_from_slice(args);
    run_cmd("ip", &argv)
}

/// Возвращает каталог per-namespace конфигурации `/etc/netns/<ns>`.
fn netns_etc_dir(ns_name: &str) -> PathBuf {
    Path::new(NETNS_ETC_ROOT).join(ns_name)
}

/// Удаляет каталог `/etc/netns/<ns>`; отсутствие — не ошибка.
pub(crate) fn remove_netns_etc(ns_name: &str) {
    match fs::remove_dir_all(netns_etc_dir(ns_name)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(%e, "не удалось удалить каталог /etc/netns/{ns_name}"),
    }
}

/// Выставляет точные права: режим, заданный при создании, маскируется umask.
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<(), AgentError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Выставляет точные права — no-op вне unix.
#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) -> Result<(), AgentError> {
    Ok(())
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

    #[test]
    fn netns_etc_dir_layout() {
        assert_eq!(
            netns_etc_dir("bottle-office-a"),
            std::path::PathBuf::from("/etc/netns/bottle-office-a")
        );
    }

    #[tokio::test]
    // Гард держится через await намеренно: мьютекс сериализует тесты
    // друг с другом (каждый в своём рантайме/потоке), внутри одного
    // теста конкурентов на локе нет.
    #[allow(clippy::await_holding_lock)]
    async fn bind_bridges_partial_failure_frees_already_bound_port() {
        // Эфемерные порты контестируются с другими тестами крейта —
        // см. test_support::EPHEMERAL_PORT_LOCK.
        let _ports = crate::test_support::EPHEMERAL_PORT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Сквоттер на wildcard-адресе: бинд моста на тот же wildcard
        // упрётся в активный слушатель и на Linux, и на macOS.
        let squatter = TcpListener::bind(("0.0.0.0", 0))
            .await
            .expect("bind сквоттера");
        let busy_port = squatter.local_addr().expect("адрес сквоттера").port();

        // Свободный порт для первого (успешного) моста SOCKS5.
        let probe = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind probe");
        let socks5_port = probe.local_addr().expect("адрес probe").port();
        drop(probe);

        let listeners = [ForwardPort {
            port: busy_port,
            target: "10.20.30.40:3389".to_string(),
        }];
        let mut bridge_tasks = JoinSet::new();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bind_bridges(
                &mut bridge_tasks,
                socks5_port,
                &listeners,
                Ipv4Addr::new(10, 77, 1, 2),
            ),
        )
        .await
        .expect("отказ бинда не должен зависать");

        // Наружу — исходная ошибка бинда, а не что-то из очистки.
        let error = outcome.expect_err("бинд на занятый порт должен провалиться");
        assert!(
            matches!(&error, AgentError::Io(e) if e.kind() == io::ErrorKind::AddrInUse),
            "ожидалась исходная ошибка AddrInUse, получено: {error}"
        );

        // Суть фикса: успешный мост остановлен и его порт свободен
        // после возврата — повторный bind проходит. Бинд повторяется
        // ограниченное время: macOS на ~150 мс резервирует только что
        // закрытый эфемерный порт при параллельных (даже неудачных)
        // запусках процессов в системе — это transient-состояние ядра,
        // а не утечка слушателя (утёкший слушатель не освободится никогда).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if TcpListener::bind(("0.0.0.0", socks5_port)).await.is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "порт {socks5_port} успешно забинженного моста \
                 не освободился за 2 с после возврата"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}
