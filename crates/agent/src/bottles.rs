//! Полный жизненный цикл бутылок: `Up`/`Down`/`Status` (задача 11).
//!
//! [`Bottles`] объединяет слои задач 8–10 в один API control-сервера:
//! проверка id ([`netns::netns_name`] — барьер инъекций пути), слоты
//! подсетей ([`netns::SlotTable`]), namespace с relay и мостами
//! ([`netns::netns_up`]) и userspace-туннель ([`wg::tunnel_up_wg`] для
//! WireGuard, OpenVPN — spawn `openvpn` внутри namespace). Конфиг туннеля
//! и pid-файлы живут в tmpfs `/run/bottles/<id>/` и удаляются при down.
//!
//! [`boot_cleanup`] приводит систему в чистое состояние при старте агента
//! с флагом `--boot`: осиротевшие процессы, остатки namespaces, каталоги
//! конфигурации и masquerade-правило для подсети бутылок.
//!
//! Контракт конкурентности: control-сервер принимает одно соединение
//! и обрабатывает запросы последовательно (план §4.3), поэтому `up`/`down`
//! не защищают промежуточные состояния провижининга от параллельного
//! вызова с тем же id — единственный писатель.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bottle_core::profile::TunnelProtocol;
use bottle_core::protocol::{err, AgentReply, BottleStatusInfo, UpArgs};

use crate::error::AgentError;
use crate::netns::{
    netns_down, netns_name, netns_up, remove_netns_etc, run_cmd, run_in_ns, set_dns, set_mode,
    NetnsHandle, Slot, SlotTable,
};
use crate::wg::{parse_wg_config, tunnel_child_dead, tunnel_down, tunnel_up_wg};

/// Корень tmpfs-каталогов бутылок: конфиги и pid-файлы.
const RUN_BOTTLES_ROOT: &str = "/run/bottles";
/// Подсеть всех бутылок — источник masquerade-правила.
const BOTTLE_SUBNET: &str = "10.77.0.0/16";
/// Внешний интерфейс аплаенса для masquerade.
const WAN_IFACE: &str = "eth0";
/// Ожидание завершения осиротевшего процесса после SIGTERM.
#[cfg(unix)]
const TERM_WAIT: Duration = Duration::from_millis(500);
/// Пауза между проверками завершения после SIGTERM.
#[cfg(unix)]
const TERM_POLL: Duration = Duration::from_millis(50);
/// Предел ожидания готовности OpenVPN (появление tun0).
const OPENVPN_WAIT: Duration = Duration::from_secs(60);
/// Пауза между опросами tun0.
const OPENVPN_POLL: Duration = Duration::from_secs(1);

/// Живая бутылка в реестре: ручка namespace плюс instance-поколение
/// демона, поднявшего её ([`UpArgs::instance`]).
///
/// Поколение хранится здесь, а не в [`NetnsHandle`]: это метаданные
/// жизненного цикла демона, netns-слой о них не знает. `Down` чужого
/// поколения ([`err::STALE_INSTANCE`]) не получает ручку вовсе.
struct LiveBottle {
    /// Ресурсы network namespace бутылки.
    handle: NetnsHandle,
    /// Поколение демона, которому принадлежит бутылка.
    instance: u64,
}

/// Реестр живых бутылок: полный Up/Down/Status поверх слоёв 8–10.
///
/// `inner` хранит живые бутылки по идентификатору, `slots` — занятость
/// сетевых слотов. std-мьютексы с короткими критическими секциями:
/// под локом нет ни одного `await`.
pub struct Bottles {
    /// Живые бутылки по идентификатору.
    inner: Mutex<HashMap<String, LiveBottle>>,
    /// Таблица занятости слотов подсетей.
    slots: Mutex<SlotTable>,
}

impl Bottles {
    /// Создаёт пустой реестр.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            slots: Mutex::new(SlotTable::default()),
        }
    }

    /// Помещает «живую» бутылку в реестр без провижининга — тестовый
    /// шов для проверки instance-семантики `Down` без root и namespaces
    /// (unit-тесты `bottles`/`control`; root-стенды поднимают настоящие
    /// бутылки через `up`).
    #[cfg(test)]
    pub fn insert_for_test(&self, id: &str, instance: u64) {
        lock(&self.inner).insert(
            id.to_string(),
            LiveBottle {
                handle: dummy_handle(id),
                instance,
            },
        );
    }

    /// Поднимает бутылку по аргументам `Up`: namespace + relay + мосты +
    /// туннель. Успех — [`AgentReply::Ready`]; любая неудача — полный
    /// разбор сделанного и [`AgentReply::Error`] с кодом
    /// [`map_agent_error`].
    ///
    /// Порядок (предписан): барьер id; живой id → `ALREADY_UP` (поколение
    /// живой бутылки не перезаписывается — её `Down` останется за
    /// владельцем); выделение слота; `set_dns` (до туннеля — DNS-endpoint
    /// WireGuard резолвится в namespace через `/etc/netns/<ns>/resolv.conf`);
    /// `netns_up`; конфиг в tmpfs; туннель; `Ready`. Поколение
    /// [`UpArgs::instance`] сохраняется вместе с бутылкой.
    pub async fn up(&self, args: UpArgs) -> AgentReply {
        // Барьер идентификатора: id попадает в пути /run/netns, /etc/netns
        // и /run/bottles — "../evil" отсекается до любых обращений к системе.
        if let Err(error) = netns_name(&args.id) {
            return map_agent_error(error);
        }
        if lock(&self.inner).contains_key(&args.id) {
            return error_reply(err::ALREADY_UP, &format!("бутылка {} уже поднята", args.id));
        }
        let slot = match lock(&self.slots).alloc() {
            Ok(slot) => slot,
            Err(error) => return map_agent_error(error),
        };

        match self.provision(&args, slot).await {
            Ok(()) => AgentReply::Ready { id: args.id },
            Err(error) => {
                lock(&self.slots).free(slot.idx);
                map_agent_error(error)
            }
        }
    }

    /// Опускает бутылку СВОЕГО поколения: туннель, namespace (с relay
    /// и мостами), слот, tmpfs-каталог.
    ///
    /// Отсутствующий id — [`err::NOT_FOUND`]. `instance`, не равный
    /// сохранённому с бутылкой поколению, — [`err::STALE_INSTANCE`]:
    /// бутылку не трогаем вовсе (защита от TOCTOU «Down проигравшего
    /// рестарта убивает живую бутылку победителя», протокол v2).
    pub async fn down(&self, id: &str, instance: u64) -> AgentReply {
        if let Err(error) = netns_name(id) {
            return map_agent_error(error);
        }
        // Проверка и вынимание — под одним локом: чужое поколение
        // возвращает ручку в реестр атомарно, бутылка остаётся живой.
        let mut inner = lock(&self.inner);
        let entry = match inner.remove(id) {
            Some(entry) => entry,
            None => {
                return error_reply(err::NOT_FOUND, &format!("бутылка {id} не найдена"));
            }
        };
        if entry.instance != instance {
            let stored = entry.instance;
            inner.insert(id.to_string(), entry);
            drop(inner);
            return error_reply(
                err::STALE_INSTANCE,
                &format!(
                    "бутылка {id} принадлежит поколению {stored}, запрос — {instance}; \
                     бутылка не тронута"
                ),
            );
        }
        drop(inner);

        let mut handle = entry.handle;
        let slot_idx = handle.slot.idx;
        let run_dir = bottle_run_dir(id);
        tunnel_down(&mut handle);
        let outcome = netns_down(handle);
        lock(&self.slots).free(slot_idx);
        remove_run_dir(&run_dir);
        match outcome {
            Ok(()) => AgentReply::Ok,
            Err(error) => map_agent_error(error),
        }
    }

    /// Статус бутылок: одна (`Some`) или все (`None`); вывод
    /// детерминирован (сортировка по id).
    pub fn status(&self, id: Option<&str>) -> Vec<BottleStatusInfo> {
        let mut bottles: Vec<BottleStatusInfo> = lock(&self.inner)
            .iter()
            .filter(|(bottle_id, _)| id.is_none_or(|wanted| bottle_id.as_str() == wanted))
            .map(|(bottle_id, _)| BottleStatusInfo {
                id: bottle_id.clone(),
                state: "up".to_string(),
                detail: None,
            })
            .collect();
        bottles.sort_by(|a, b| a.id.cmp(&b.id));
        bottles
    }

    /// Поднимает ресурсы бутылки после выделения слота.
    ///
    /// Слот освобождает вызывающий [`Bottles::up`] при любой ошибке;
    /// эта функция отвечает за teardown всего, что успела создать сама
    /// (после успешного `netns_up` — полный down-путь).
    async fn provision(&self, args: &UpArgs, slot: Slot) -> Result<(), AgentError> {
        // DNS до namespace и туннеля: bind-mount resolv.conf применяется
        // каждым `ip netns exec`, getent внутри ns должен видеть новый
        // сервер ещё на резолве endpoint.
        if let Err(error) = set_dns(&args.id, args.dns) {
            // set_dns мог оставить каталог /etc/netns/<ns> — убираем.
            if let Ok(ns_name) = netns_name(&args.id) {
                remove_netns_etc(&ns_name);
            }
            return Err(error);
        }

        let mut handle = netns_up(&slot, &args.id, &args.forwards, args.socks5_port).await?;
        match launch_tunnel(&mut handle, args).await {
            Ok(()) => self.commit_to_registry(handle, &args.id, args.instance),
            Err(error) => {
                tracing::warn!(id = %args.id, %error, "провижининг не удался, разбираю бутылку");
                teardown(handle);
                Err(error)
            }
        }
    }

    /// Финальная вставка бутылки в реестр ПОД ЛОКОМ с перепроверкой
    /// (TOCTOU, рулинг M3-бэклога): между барьером `contains_key` в
    /// [`Bottles::up`] и концом провижининга (десятки секунд туннеля)
    /// параллельный `up` того же id мог выиграть. Победитель уже в
    /// реестре — проигравший разбирает СВОЁ (teardown) и отвечает
    /// [`AgentError::BadInstance`]; бутылка победителя не трогается.
    fn commit_to_registry(
        &self,
        handle: NetnsHandle,
        id: &str,
        instance: u64,
    ) -> Result<(), AgentError> {
        let mut inner = lock(&self.inner);
        if inner.contains_key(id) {
            drop(inner);
            tracing::warn!(
                id,
                instance,
                "параллельный up выиграл гонку: разбираю проигравшего"
            );
            teardown(handle);
            return Err(AgentError::BadInstance { id: id.to_string() });
        }
        inner.insert(id.to_string(), LiveBottle { handle, instance });
        Ok(())
    }
}

impl Default for Bottles {
    fn default() -> Self {
        Self::new()
    }
}

/// Поднимает туннель бутылки по протоколу из аргументов и пишет
/// конфиг + pid-файлы в tmpfs `/run/bottles/<id>/`.
async fn launch_tunnel(handle: &mut NetnsHandle, args: &UpArgs) -> Result<(), AgentError> {
    let run_dir = bottle_run_dir(&args.id);
    create_run_dir(&run_dir)?;
    // pid relay — сразу: агент может погибнуть во время долгого ожидания
    // туннеля, boot_cleanup должен найти процесс.
    write_pid_file(&run_dir.join("pid-relay"), handle.relay_child.id())?;

    match args.protocol {
        TunnelProtocol::WireGuard => {
            write_secret_file(&run_dir.join("wg.conf"), &args.config)?;
            let parsed = parse_wg_config(&args.config)?;
            tunnel_up_wg(handle, parsed).await?;
        }
        TunnelProtocol::OpenVpn => {
            let config_path = run_dir.join("openvpn.conf");
            write_secret_file(&config_path, &args.config)?;
            openvpn_up(handle, &config_path).await?;
        }
    }

    if let Some(child) = &handle.tunnel_child {
        write_pid_file(&run_dir.join("pid-tunnel"), child.id())?;
    }
    Ok(())
}

/// Поднимает OpenVPN-туннель внутри namespace бутылки.
///
/// Конфиг уже записан в `config_path` (общая файловая система — процесс
/// в namespace видит тот же файл). Готовность — появление интерфейса
/// `tun0` (≤60 с, шаг 1 с): поллинг `ip link show tun0`; смерть процесса
/// и таймаут — [`AgentError::TunnelFailed`].
async fn openvpn_up(handle: &mut NetnsHandle, config_path: &Path) -> Result<(), AgentError> {
    let ns = handle.ns_name.clone();
    let config = config_path
        .to_str()
        .ok_or_else(|| AgentError::InvalidConfig("путь конфига openvpn не UTF-8".into()))?
        .to_string();

    let child = Command::new("ip")
        .args([
            "netns",
            "exec",
            &ns,
            "openvpn",
            "--config",
            &config,
            "--dev",
            "tun0",
            "--dev-type",
            "tun",
        ])
        // Лог openvpn наследует stdio агента — у пользователей нет другого
        // окна диагностики «Initialization Sequence Completed».
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    handle.tunnel_child = Some(child);

    let deadline = Instant::now() + OPENVPN_WAIT;
    loop {
        if run_in_ns(&ns, &["ip", "link", "show", "tun0"]).is_ok() {
            return Ok(());
        }
        if tunnel_child_dead(handle) {
            return Err(AgentError::TunnelFailed(
                "процесс openvpn завершился до создания tun0".into(),
            ));
        }
        if Instant::now() + OPENVPN_POLL > deadline {
            return Err(AgentError::TunnelFailed(format!(
                "интерфейс tun0 не появился за {} с",
                OPENVPN_WAIT.as_secs()
            )));
        }
        tokio::time::sleep(OPENVPN_POLL).await;
    }
}

/// Полный разбор бутылки по down-пути (используется при неудаче up):
/// туннель → namespace (relay, мосты) → tmpfs-каталог.
fn teardown(mut handle: NetnsHandle) {
    let run_dir = bottle_run_dir(&handle.id);
    tunnel_down(&mut handle);
    if let Err(error) = netns_down(handle) {
        tracing::warn!(%error, "не удалось полностью разобрать бутылку после неудачи");
    }
    remove_run_dir(&run_dir);
}

/// Приводит систему в чистое состояние при старте агента (`--boot`):
///
/// 1. убивает осиротевшие relay/туннели по pid-файлам `/run/bottles/*/`
///    (после падения агента их некому `kill()`); pid-файлы переживают
///    рестарт агента без перезагрузки VM, поэтому перед сигналом
///    проверяется cmdline процесса — реюзнутый чужой pid не трогается
///    ([`should_kill_pid`]);
/// 2. удаляет все namespaces `bottle-*` (с добиванием выживших процессов
///    — живой процесс удержал бы namespace после `ip netns del`);
/// 3. удаляет `/etc/netns/bottle-*` и `/run/bottles/*`;
/// 4. ставит masquerade для подсети бутылок (один раз на boot — SNAT
///    пакетов туннелей при выходе из VM через eth0).
///
/// Идемпотентна: на пустом состоянии ничего не делает (кроме
/// masquerade-проверки). Ошибки отдельных шагов логируются и не прерывают
/// очистку.
pub fn boot_cleanup() {
    kill_pidfile_orphans();
    delete_bottle_netns();
    remove_matching_dirs(Path::new("/etc/netns"), "bottle-");
    remove_matching_dirs(Path::new(RUN_BOTTLES_ROOT), "");
    ensure_masquerade();
}

/// Убивает процессы из pid-файлов `/run/bottles/*/pid-{relay,tunnel}` —
/// только те, чья cmdline опознана как процесс бутылки
/// ([`should_kill_pid`]): /run — tmpfs, pid-файлы переживают рестарт
/// агента без перезагрузки VM, и pid мог быть реюзнут чужим процессом.
fn kill_pidfile_orphans() {
    let Ok(entries) = fs::read_dir(RUN_BOTTLES_ROOT) else {
        return; // пустое состояние — норма
    };
    for entry in entries.flatten() {
        for pid_file in ["pid-relay", "pid-tunnel"] {
            let path = entry.path().join(pid_file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(pid) = text.trim().parse::<i32>() else {
                tracing::warn!(file = %path.display(), "повреждённый pid-файл, пропущен");
                continue;
            };
            match read_cmdline(pid) {
                Some(cmdline) if should_kill_pid(&cmdline) => kill_orphan(pid),
                Some(cmdline) => tracing::warn!(
                    pid,
                    ?cmdline,
                    "pid из pid-файла не похож на процесс бутылки (реюз pid?) — не тронут"
                ),
                // Нет /proc или процесса — сигнал по слепому pid из файла
                // не отправляем: это чужая территория.
                None => tracing::warn!(pid, "cmdline процесса недоступна — pid не тронут"),
            }
        }
    }
}

/// Опознаёт cmdline как процесс бутылки: relay — это сам бинарник
/// агента (`bottle-agent relay --socks5-port …`: `ip netns exec`
/// выполняет команду через execvp без промежуточного процесса, см.
/// [`crate::netns`]), туннели — `wireguard-go wg<idx>`
/// ([`crate::wg::tunnel_up_wg`]) и `openvpn --config …`
/// ([`openvpn_up`]).
///
/// Прагматика (по брифингу T12): матчинг по подстроке всей cmdline,
/// без разбора argv — маркеры достаточно специфичны для аплаенса.
fn should_kill_pid(cmdline: &str) -> bool {
    cmdline.contains("bottle-agent")
        || cmdline.contains("wireguard-go")
        || cmdline.contains("openvpn")
}

/// Читает `/proc/<pid>/cmdline` (argv через NUL; NUL допустим в UTF-8,
/// поэтому lossy-конвертация меняет только экзотические байты).
/// Отсутствие `/proc` или процесса — `None`.
fn read_cmdline(pid: i32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// SIGTERM → короткое ожидание → SIGKILL для осиротевшего процесса.
///
/// Это не дочерний процесс текущего агента: waitpid невозможен,
/// завершение отслеживается сигналом 0. Вызывающий из pid-файла
/// обязан сначала отсечь реюз pid проверкой cmdline
/// ([`should_kill_pid`]); `delete_bottle_netns` зовёт без проверки —
/// её pid'ы принадлежность namespace'у уже доказали (`ip netns pids`).
#[cfg(unix)]
fn kill_orphan(pid: i32) {
    if !pid_alive(pid) {
        return;
    }
    // SAFETY: kill(2) по конкретному pid; побочных эффектов на память нет.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + TERM_WAIT;
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(TERM_POLL);
    }
    if pid_alive(pid) {
        // SAFETY: см. выше.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Вне unix завершение по сигналу недоступно; агент работает только
/// в Linux-аплаенсе, ветка существует для компиляции.
#[cfg(not(unix))]
fn kill_orphan(pid: i32) {
    tracing::warn!(pid, "завершение процессов не поддерживается вне unix");
}

/// Жив ли процесс (сигнал 0 — проверка без доставки).
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(2) с сигналом 0 не доставляет сигнал процессу.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Удаляет все namespaces `bottle-*` из `ip netns list`.
///
/// Перед удалением добиваются процессы внутри namespace: pid-файлы
/// покрывают не всё (крах агента между spawn и записью файла), а живой
/// процесс удерживает namespace живым после `ip netns del`.
fn delete_bottle_netns() {
    let Ok(list) = run_cmd("ip", &["netns", "list"]) else {
        tracing::warn!("ip netns list не выполнен — namespaces не очищены");
        return;
    };
    for name in list
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.starts_with("bottle-"))
    {
        if let Ok(pids) = run_cmd("ip", &["netns", "pids", name]) {
            for pid in pids
                .split_whitespace()
                .filter_map(|t| t.parse::<i32>().ok())
            {
                kill_orphan(pid);
            }
        }
        if let Err(error) = run_cmd("ip", &["netns", "del", name]) {
            tracing::warn!(%error, "не удалось удалить namespace {name}");
        }
    }
}

/// Удаляет подкаталоги `root` с именами на `prefix` (пустой prefix — все).
fn remove_matching_dirs(root: &Path, prefix: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return; // нет каталога — нечего чистить
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            if let Err(error) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(%error, "не удалось удалить {}", entry.path().display());
            }
        }
    }
}

/// Ставит masquerade-правило подсети бутылок, если его ещё нет
/// (эквивалент `iptables -C ... || iptables -A ...`).
fn ensure_masquerade() {
    let check = [
        "-t",
        "nat",
        "-C",
        "POSTROUTING",
        "-s",
        BOTTLE_SUBNET,
        "-o",
        WAN_IFACE,
        "-j",
        "MASQUERADE",
    ];
    if run_cmd("iptables", &check).is_ok() {
        return;
    }
    let add = [
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        BOTTLE_SUBNET,
        "-o",
        WAN_IFACE,
        "-j",
        "MASQUERADE",
    ];
    if let Err(error) = run_cmd("iptables", &add) {
        tracing::warn!(%error, "не удалось установить masquerade для {BOTTLE_SUBNET}");
    }
}

/// Маппинг ошибки агента в ответ `Error`: демон (задача 16) по
/// [`err::PORT_BUSY`] делает порт-ретрай, поэтому `AddrInUse` обязан
/// попадать точно в него.
pub fn map_agent_error(error: AgentError) -> AgentReply {
    let code = match &error {
        AgentError::InvalidId
        | AgentError::NoFreeSlots
        | AgentError::CommandFailed { .. }
        | AgentError::InvalidConfig(_) => err::BAD_REQUEST,
        AgentError::TunnelFailed(_) => err::TUNNEL_FAILED,
        // Проигравший гонки up: бутылка-победитель ЖИВА — семантика
        // «уже поднята» для демона честнее, чем «плохой запрос».
        AgentError::BadInstance { .. } => err::ALREADY_UP,
        AgentError::Io(e) if e.kind() == io::ErrorKind::AddrInUse => err::PORT_BUSY,
        AgentError::Io(_) => err::BAD_REQUEST,
    };
    AgentReply::Error {
        code: code.to_string(),
        message: error.to_string(),
    }
}

/// Возвращает tmpfs-каталог бутылки `/run/bottles/<id>`.
///
/// Вызывается только для id, прошедшего [`netns_name`].
fn bottle_run_dir(id: &str) -> PathBuf {
    Path::new(RUN_BOTTLES_ROOT).join(id)
}

/// Создаёт tmpfs-каталог бутылки с правами 0700 root.
fn create_run_dir(dir: &Path) -> Result<(), AgentError> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)
}

/// Пишет секретный файл (конфиг туннеля) с правами 0600.
fn write_secret_file(path: &Path, content: &str) -> Result<(), AgentError> {
    fs::write(path, content)?;
    set_mode(path, 0o600)
}

/// Пишет pid-файл процесса бутылки.
fn write_pid_file(path: &Path, pid: u32) -> Result<(), AgentError> {
    fs::write(path, format!("{pid}\n"))?;
    set_mode(path, 0o600)
}

/// Удаляет tmpfs-каталог бутылки; отсутствие — не ошибка.
fn remove_run_dir(dir: &Path) {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(%e, "не удалось удалить каталог {}", dir.display()),
    }
}

/// Блокирует мьютекс, восстанавливая данные после отравления:
/// паника в короткой критической секции не оставляет инвариантов.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ручка namespace без реальных ресурсов для тестового шва
/// [`Bottles::insert_for_test`]: короткоживущий процесс вместо relay,
/// без туннеля и мостов.
#[cfg(test)]
fn dummy_handle(id: &str) -> NetnsHandle {
    NetnsHandle {
        id: id.to_string(),
        ns_name: format!("bottle-{id}"),
        slot: Slot::new(1),
        // «true» мгновенно завершается — down-путь получит уже мёртвый
        // процесс, kill/wait которого безопасны (ESRCH логируется).
        relay_child: Command::new("true")
            .spawn()
            .expect("запустить процесс-заглушку"),
        tunnel_child: None,
        bridge_tasks: tokio::task::JoinSet::new(),
    }
}

/// Собирает ответ-ошибку.
fn error_reply(code: &str, message: &str) -> AgentReply {
    AgentReply::Error {
        code: code.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_invalid_id_is_bad_request() {
        assert_eq!(
            map_agent_error(AgentError::InvalidId).code(),
            err::BAD_REQUEST
        );
    }

    #[test]
    fn map_no_free_slots_is_bad_request() {
        assert_eq!(
            map_agent_error(AgentError::NoFreeSlots).code(),
            err::BAD_REQUEST
        );
    }

    #[test]
    fn map_command_failed_is_bad_request() {
        let error = AgentError::CommandFailed {
            command: "ip netns add bottle-x".into(),
            stderr: "File exists".into(),
        };
        let reply = map_agent_error(error);
        assert_eq!(reply.code(), err::BAD_REQUEST);
        assert!(
            reply.message().contains("ip netns add bottle-x"),
            "сообщение сохраняет контекст команды"
        );
    }

    #[test]
    fn map_invalid_config_is_bad_request() {
        let reply = map_agent_error(AgentError::InvalidConfig("нет ключа Endpoint".into()));
        assert_eq!(reply.code(), err::BAD_REQUEST);
    }

    #[test]
    fn map_tunnel_failed_is_tunnel_failed() {
        let reply = map_agent_error(AgentError::TunnelFailed("handshake не произошёл".into()));
        assert_eq!(reply.code(), err::TUNNEL_FAILED);
    }

    #[test]
    fn map_addr_in_use_is_port_busy() {
        let reply = map_agent_error(AgentError::Io(io::Error::from(io::ErrorKind::AddrInUse)));
        assert_eq!(
            reply.code(),
            err::PORT_BUSY,
            "демон делает порт-ретрай по PORT_BUSY — маппинг точный"
        );
    }

    #[test]
    fn map_other_io_is_bad_request_with_message() {
        let reply = map_agent_error(AgentError::Io(io::Error::other("отказано в доступе")));
        assert_eq!(reply.code(), err::BAD_REQUEST);
        assert!(reply.message().contains("отказано в доступе"));
    }

    #[tokio::test]
    async fn up_rejects_path_traversal_before_any_syscall() {
        let bottles = Bottles::new();
        let args = UpArgs {
            id: "../evil".to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
            dns: None,
            socks5_port: 0,
            forwards: vec![],
            instance: 0,
        };
        let reply = bottles.up(args).await;
        assert_eq!(reply.code(), err::BAD_REQUEST, "барьер id на уровне API");
        assert!(
            bottles.status(None).is_empty(),
            "неудавшийся up не оставляет бутылок"
        );
    }

    #[tokio::test]
    async fn up_rejects_non_ascii_id() {
        let bottles = Bottles::new();
        let args = UpArgs {
            id: "офис".to_string(),
            protocol: TunnelProtocol::WireGuard,
            config: String::new(),
            dns: None,
            socks5_port: 0,
            forwards: vec![],
            instance: 0,
        };
        let reply = bottles.up(args).await;
        assert_eq!(reply.code(), err::BAD_REQUEST);
    }

    /// TOCTOU реестра (рулинг M3-бэклога): провижининг длится десятки
    /// секунд; параллельный `up` того же id может пройти барьер
    /// `contains_key` и выиграть. Проигравший после своего туннеля
    /// обязан УВИДЕТЬ чужую запись, разобрать своё и ответить
    /// BadInstance — не перезаписать бутылку победителя.
    #[test]
    fn commit_to_registry_refuses_when_parallel_up_won() {
        let bottles = Bottles::new();
        // Победитель: успел вставить свою бутылку (instance 7).
        lock(&bottles.inner).insert(
            "office-a".to_string(),
            LiveBottle {
                handle: dummy_handle("office-a"),
                instance: 7,
            },
        );

        // Проигравший завершил провижининг своего экземпляра (instance 9).
        let outcome = bottles.commit_to_registry(dummy_handle("office-a"), "office-a", 9);

        assert!(
            matches!(&outcome, Err(AgentError::BadInstance { id }) if id == "office-a"),
            "проигравший — BadInstance, получено {outcome:?}"
        );
        // Реестр не тронут: бутылка победителя на месте, одна штука.
        let inner = lock(&bottles.inner);
        assert_eq!(inner.len(), 1);
        assert_eq!(inner["office-a"].instance, 7, "победитель не перезаписан");
    }

    /// Мирный путь: реестр пуст — вставка проходит.
    #[test]
    fn commit_to_registry_inserts_when_free() {
        let bottles = Bottles::new();

        let outcome = bottles.commit_to_registry(dummy_handle("office-a"), "office-a", 3);

        assert!(outcome.is_ok());
        let inner = lock(&bottles.inner);
        assert_eq!(inner["office-a"].instance, 3);
    }

    #[tokio::test]
    async fn down_unknown_id_is_not_found() {
        let bottles = Bottles::new();
        let reply = bottles.down("ghost", 0).await;
        assert_eq!(reply.code(), err::NOT_FOUND);
    }

    #[tokio::test]
    async fn down_with_foreign_instance_is_stale_and_bottle_stays_alive() {
        let bottles = Bottles::new();
        bottles.insert_for_test("office-a", 7);

        let reply = bottles.down("office-a", 8).await;
        assert_eq!(
            reply.code(),
            err::STALE_INSTANCE,
            "чужое поколение — код STALE_INSTANCE"
        );

        // Бутылка жива: реестр не тронут (любое чужое поколение — ниже
        // или выше сохранённого — одинаково отвергается).
        for foreign in [0u64, 6, 8, u64::MAX] {
            let reply = bottles.down("office-a", foreign).await;
            assert_eq!(reply.code(), err::STALE_INSTANCE, "instance {foreign}");
        }
        assert_eq!(
            bottles.status(Some("office-a")),
            vec![BottleStatusInfo {
                id: "office-a".to_string(),
                state: "up".to_string(),
                detail: None,
            }],
            "бутылка чужого поколения не опускается"
        );
    }

    #[tokio::test]
    async fn down_with_own_instance_removes_bottle_from_registry() {
        let bottles = Bottles::new();
        bottles.insert_for_test("office-a", 7);

        // Свой instance — бутылка разбирается. Teardown выполняет
        // системные команды (ip netns del и др.), которые без root
        // завершаются ошибкой — значим факт удаления из реестра,
        // а не код ответа.
        let _reply = bottles.down("office-a", 7).await;
        assert!(
            bottles.status(Some("office-a")).is_empty(),
            "бутылка своего поколения удалена из реестра"
        );
    }

    #[test]
    fn status_of_empty_registry_is_empty() {
        let bottles = Bottles::new();
        assert_eq!(bottles.status(None), vec![]);
        assert_eq!(bottles.status(Some("any")), vec![]);
    }

    #[test]
    fn run_dir_layout_follows_id() {
        assert_eq!(
            bottle_run_dir("office-a"),
            PathBuf::from("/run/bottles/office-a")
        );
    }

    #[test]
    fn boot_cleanup_survives_empty_state() {
        // На пустом состоянии (нет /run/bottles, нет bottle-* namespaces,
        // недоступны ip/iptables) очистка завершается без паники —
        // все шаги идемпотентны, ошибки отдельных команд логируются.
        boot_cleanup();
    }

    /// Собирает cmdline как в `/proc/<pid>/cmdline`: argv через NUL.
    fn cmdline(parts: &[&str]) -> String {
        parts.join("\0")
    }

    #[test]
    fn should_kill_pid_matches_relay_cmdline() {
        // ip netns exec execvp'ит команду без fork: ребёнок — сам бинарник
        // агента с аргументами relay (netns::spawn_relay).
        let relay = cmdline(&[
            "/usr/local/bin/bottle-agent",
            "relay",
            "--socks5-port",
            "10800",
            "--forward",
            "33389:10.20.30.15:3389",
        ]);
        assert!(
            should_kill_pid(&relay),
            "наш relay должен признаваться своим"
        );
    }

    #[test]
    fn should_kill_pid_matches_tunnel_cmdlines() {
        // Туннельные дети (wg::tunnel_up_wg, openvpn_up) — argv запуска
        // внутри namespace после execvp.
        let wg = cmdline(&["/usr/bin/wireguard-go", "wg3"]);
        assert!(
            should_kill_pid(&wg),
            "wireguard-go должен признаваться своим"
        );
        let ovpn = cmdline(&[
            "openvpn",
            "--config",
            "/run/bottles/office-a/openvpn.conf",
            "--dev",
            "tun0",
            "--dev-type",
            "tun",
        ]);
        assert!(should_kill_pid(&ovpn), "openvpn должен признаваться своим");
    }

    #[test]
    fn should_kill_pid_rejects_unrelated_processes() {
        // Ядро сценария: после рестарта агента без перезагрузки VM pid
        // из старого pid-файла мог достаться чужому процессу.
        for parts in [
            &["nginx", "-g", "daemon off;"][..],
            &["python3", "server.py"][..],
            &["systemd", "--switched-root", "--system"][..],
            &["sshd", "-D", "-e"][..],
            &["kubelet", "--config=/etc/kubernetes/kubelet.yaml"][..],
        ] {
            let foreign = cmdline(parts);
            assert!(
                !should_kill_pid(&foreign),
                "чужой процесс не должен убиваться: {foreign:?}"
            );
        }
    }

    #[test]
    fn should_kill_pid_rejects_empty_cmdline() {
        // cmdline потоков ядра пуста — их нельзя трогать ни при каких
        // pid-файлах.
        assert!(!should_kill_pid(""));
    }

    #[test]
    fn read_cmdline_of_dead_pid_is_none() {
        // Несуществующий pid: на Linux нет /proc/<pid>/cmdline, вне Linux
        // нет /proc вовсе — оба случая дают None (pid не тронут).
        assert_eq!(read_cmdline(i32::MAX), None);
    }

    /// Тренажёр ответа-ошибки для ассертов на код/сообщение.
    trait ReplyCode {
        fn code(&self) -> &str;
        fn message(&self) -> &str;
    }

    impl ReplyCode for AgentReply {
        fn code(&self) -> &str {
            match self {
                AgentReply::Error { code, .. } => code,
                other => panic!("ожидалась ошибка, получено {other:?}"),
            }
        }

        fn message(&self) -> &str {
            match self {
                AgentReply::Error { message, .. } => message,
                other => panic!("ожидалась ошибка, получено {other:?}"),
            }
        }
    }
}
