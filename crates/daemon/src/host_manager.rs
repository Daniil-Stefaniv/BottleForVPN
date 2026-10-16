//! Host-менеджер демона (M2 T9/T10): жизненный цикл host-бутылок, спека §7.2.
//!
//! Старт: проверка службы маршрутов (`svc_pipe_exists`, отсутствие —
//! текст установки T7) → резолв endpoint'ов (спека §9: v6-endpoint без
//! v6-шлюза физики при отсутствии v4 блокирует запуск) → svc.add
//! точечных маршрутов для endpoint'ов ВСЕХ активных host-бутылок (вкл.
//! чужие — маршруты обязаны стоять до запуска клиента) → снапшот
//! адаптеров → запуск фирменного клиента → поллинг появления
//! VPN-интерфейса (явный `interface` или единственный новый `Up`,
//! ≤120 с) → релеи на `127.0.0.1` с исходящими сокетами, привязанными
//! к адаптеру VPN (`IP_UNICAST_IF`/`IPV6_UNICAST_IF` ДО connect —
//! Review Focus #5) → `up`. Любая ошибка — полный откат своих
//! маршрутов; спавн-процесс клиента не завершается (нет API, спека §7.2).
//!
//! DNS-цикл (T10, шаг 5 спеки): каждые 30 с endpoint'ы активных сессий
//! re-resolve'ятся, новые адреса получают маршруты, прежние живут в
//! живом множестве до stop (не рвать установленное соединение).
//! `stop`: релеи вниз, svc.del всех адресов сессии, клиент остаётся
//! работать — предупреждение в ответе CLI.
//!
//! Все эффекты вне процесса идут через inject-трейты ([`AdapterSource`],
//! [`ClientSpawner`], [`SvcFactory`], [`GatewaySource`],
//! [`EndpointResolver`], [`IfaceBinder`]) — юнит-тесты гоняют машину
//! состояний на фейках (macOS); реальное Windows-поведение — msvc-check
//! + ручной чеклист (Task 16).
//!
//! Клиент службы маршрутов пересоздаётся при ошибках транспорта
//! (poison-политика T7: таймаут отравляет соединение, опоздавший ответ
//! не должен удовлетворить следующий запрос) — [`HostManager::try_svc`].

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bottle_core::ipc::{BottleState, CliReply, RuntimeState};
use bottle_core::profile::{BottleProfile, VpnConfig};
use relay_lib::forward::{serve_forward_with_dial, Dial, DialFuture, TargetAddr};
use relay_lib::socks5::serve_socks5_with_dial;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;

use crate::error::DaemonError;
use crate::host_net::{resolve_endpoints_async, GwFamily};
use crate::launch::{spawn_launch, split_command_line};
use crate::manager::{reserve_ports, HostListeners, RelayDeath};
use crate::svc_client::{svc_pipe_exists, SvcClient, SVC_MISSING_MESSAGE};

/// Сколько ждать появления VPN-интерфейса после запуска клиента
/// (спека §7.2, шаг 3).
pub const IFACE_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Шаг поллинга адаптеров при ожидании интерфейса.
pub const IFACE_POLL_STEP: Duration = Duration::from_secs(2);
/// Период фоновой DNS-перепроверки endpoint'ов активных host-бутылок
/// (спека §7.2, шаг 5).
pub const DNS_RECHECK_STEP: Duration = Duration::from_secs(30);
/// Предупреждение в ответе `stop` host-бутылки: фирменный клиент
/// остаётся работать (API завершения нет — спека §7.2, шаг 6).
const HOST_STOP_CLIENT_WARNING: &str =
    "VPN-клиент оставлен запущенным (API завершения у вендорских клиентов нет); \
     остановите его вручную, если он больше не нужен";
// ----------------------------------------------------------------------
// Инъекционные зависимости (тестируемость на фейках, macOS).
// ----------------------------------------------------------------------

/// Сетевой адаптер из снапшота `Get-NetAdapter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAdapter {
    /// `InterfaceAlias` — имя адаптера (маршруты службы, выбор интерфейса).
    pub alias: String,
    /// `InterfaceIndex` — индекс адаптера (привязка сокетов, исключения
    /// шлюза).
    pub idx: u32,
    /// Адаптер `Up` (статус `Status -eq 'Up'`).
    pub up: bool,
}

/// Источник снапшотов сетевых адаптеров (`Get-NetAdapter`); инъекция для
/// тестов (фальшивые «появления» VPN-адаптера по расписанию).
#[async_trait]
pub trait AdapterSource: Send + Sync {
    /// Возвращает текущий список адаптеров.
    async fn list_adapters(&self) -> io::Result<Vec<NetAdapter>>;
}

/// Запускает фирменный VPN-клиент; процесс НЕ ожидается (клиент живёт
/// своей жизнью, завершать его демону нечем — спека §7.2).
#[async_trait]
pub trait ClientSpawner: Send + Sync {
    /// Запускает `client` с аргументами `args`.
    async fn spawn(&self, client: &str, args: &[String]) -> io::Result<()>;
}

/// Продакшн-запуск клиента: `Command::spawn` с заглушёнными stdio
/// (клиент отсоединён от демона, вывод не собирается).
pub struct RealClientSpawner;

#[async_trait]
impl ClientSpawner for RealClientSpawner {
    async fn spawn(&self, client: &str, args: &[String]) -> io::Result<()> {
        let mut command = std::process::Command::new(client);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Демон может быть запущен из GUI без консоли: без этого флага
        // спавн консольного клиента показывал бы окно консоли.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        command.spawn().map(|child| {
            // Аргументы клиентов маскируются целиком (спека §10).
            tracing::info!(
                client,
                args = %crate::logging::mask_args(&args.join(" ")),
                pid = child.id(),
                "VPN-клиент запущен"
            );
        })
    }
}

/// Фабрика клиентов службы маршрутов: каждое [`SvcFactory::create`] —
/// новое соединение и сессия (`Hello`). Poison-политика T9: транспорт,
/// отравленный таймаутом/разрывом, требует пересоздания клиента —
/// менеджер кэширует клиент до первой io-ошибки и обращается к фабрике
/// заново.
#[async_trait]
pub trait SvcFactory: Send + Sync {
    /// Открывает новую сессию службы маршрутов.
    async fn create(&self) -> Result<Arc<SvcClient>, DaemonError>;
}

/// Источник физических шлюзов ОС по семейству; инъекция для тестов
/// (v6-блокировка §9, «шлюз не найден»).
#[async_trait]
pub trait GatewaySource: Send + Sync {
    /// Физический шлюз семейства `family` с учётом исключаемых
    /// индексов адаптеров (адаптеры host-бутылок).
    async fn gateway(
        &self,
        exclude_idx: &[u32],
        family: GwFamily,
    ) -> Result<(IpAddr, String), DaemonError>;
}

/// Продакшн-шлюзы: [`crate::host_net::physical_gateway`].
pub struct RealGateways;

#[async_trait]
impl GatewaySource for RealGateways {
    async fn gateway(
        &self,
        exclude_idx: &[u32],
        family: GwFamily,
    ) -> Result<(IpAddr, String), DaemonError> {
        crate::host_net::physical_gateway(exclude_idx, family).await
    }
}

/// Резолвер endpoint-хостов; инъекция для таймлайн-тестов DNS-цикла.
#[async_trait]
pub trait EndpointResolver: Send + Sync {
    /// Резолвит хосты в адреса (дедуп делает вызывающий).
    async fn resolve(&self, hosts: &[String]) -> Result<Vec<IpAddr>, DaemonError>;
}

/// Продакшн-резолвер: системный резолвер через [`resolve_endpoints_async`].
pub struct RealEndpointResolver;

#[async_trait]
impl EndpointResolver for RealEndpointResolver {
    async fn resolve(&self, hosts: &[String]) -> Result<Vec<IpAddr>, DaemonError> {
        resolve_endpoints_async(hosts).await
    }
}

// ----------------------------------------------------------------------
// Привязка исходящих сокетов к адаптеру VPN (Review Focus #5).
// ----------------------------------------------------------------------

/// Привязка исходящего сокета релея к адаптеру VPN ДО connect; инъекция
/// для пина порядка «set → connect» на фейке.
pub trait IfaceBinder: Send + Sync {
    /// Привязывает сокет `sock` к интерфейсу `ifindex` (v6-цель —
    /// `IPV6_UNICAST_IF`, иначе `IP_UNICAST_IF`).
    fn bind(&self, sock: &socket2::Socket, ifindex: u32, v6: bool) -> io::Result<()>;
}

/// Продакшн-привязка: [`bind_to_iface`].
pub struct RealIfaceBinder;

impl IfaceBinder for RealIfaceBinder {
    fn bind(&self, sock: &socket2::Socket, ifindex: u32, v6: bool) -> io::Result<()> {
        bind_to_iface(sock, ifindex, v6)
    }
}

/// Значение опции `IP_UNICAST_IF`/`IPV6_UNICAST_IF` для индекса
/// интерфейса. Windows (MSDN IPPROTO_IP / IPPROTO_IPV6): IPv4-вариант
/// принимает индекс в сетевом порядке байтов, IPv6 — в хостовом.
pub fn unicast_if_value(ifindex: u32, v6: bool) -> u32 {
    if v6 {
        ifindex
    } else {
        ifindex.to_be()
    }
}

/// Привязывает исходящий сокет к интерфейсу `ifindex`: `IP_UNICAST_IF`
/// (v4) / `IPV6_UNICAST_IF` (v6) — исходящий трафик релея покидает
/// машину через адаптер VPN вопреки борьбе за default-маршрут
/// (Review Focus #5). Вызывать ДО connect.
///
/// socket2 0.6 не экспортирует эти опции (проверено по исходникам
/// крейта: есть только unix `bind_device_by_index_*` за feature «all»),
/// поэтому Windows использует сырой `setsockopt` (windows-sys) на
/// дескрипторе сокета socket2 — рулинг T9 допускает raw setsockopt.
/// Не-Windows — отказ: host-стек демона (служба маршрутов,
/// `Get-NetAdapter`) существует только на Windows, сюда код на других
/// платформах не доходит.
pub fn bind_to_iface(sock: &socket2::Socket, ifindex: u32, v6: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            setsockopt, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, IP_UNICAST_IF,
        };
        let (level, option) = if v6 {
            (IPPROTO_IPV6, IPV6_UNICAST_IF)
        } else {
            (IPPROTO_IP, IP_UNICAST_IF)
        };
        let value = unicast_if_value(ifindex, v6);
        let result = unsafe {
            setsockopt(
                // SOCKET в windows-sys — usize; RawSocket std — u64
                // (x86_64 — преобразование без потерь).
                sock.as_raw_socket() as usize,
                level,
                option,
                (&value as *const u32).cast(),
                std::mem::size_of::<u32>() as i32,
            )
        };
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (sock, ifindex, v6);
        Err(io::Error::other(
            "привязка исходящих сокетов к интерфейсу доступна только на Windows",
        ))
    }
}

/// Код `EINPROGRESS` целевой unix-платформы: неблокирующий `connect`
/// стартует соединение и возвращается с этой ошибкой (норма, ждём
/// готовности). std не мапит её в `ErrorKind::WouldBlock`, поэтому
/// сверяем сырой код. Windows отдаёт `WSAEWOULDBLOCK`, который std
/// мапит в `WouldBlock` — отдельный код не нужен.
#[cfg(target_os = "macos")]
const EINPROGRESS: i32 = 36;
#[cfg(target_os = "linux")]
const EINPROGRESS: i32 = 115;

/// Устанавливает исходящее соединение с привязкой к интерфейсу ДО
/// connect: сокет создаётся через socket2, [`IfaceBinder::bind`]
/// применяется до начала соединения, неблокирующий connect ждёт
/// готовности через tokio (`writable` + `SO_ERROR`).
pub async fn dial_bound(
    binder: &dyn IfaceBinder,
    ifindex: u32,
    addr: SocketAddr,
) -> io::Result<TcpStream> {
    let sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    sock.set_nonblocking(true)?;
    sock.set_tcp_nodelay(true)?;
    // Review Focus #5: привязка строго ДО connect.
    binder.bind(&sock, ifindex, addr.is_ipv6())?;
    let peer = socket2::SockAddr::from(addr);
    match sock.connect(&peer) {
        Ok(()) => {}
        // Неблокирующий connect в прогрессе (WouldBlock на Windows,
        // EINPROGRESS на unix) — дождаться готовности сокета.
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(EINPROGRESS) => {}
        Err(error) => return Err(error),
    }
    let stream = TcpStream::from_std(sock.into())?;
    // Готовность опрашиваем try_write, а не writable().await: на Windows
    // событие завершения connect одноразовое и могло наступить ДО
    // регистрации сокета в реакторе (конвертация из socket2) — тогда
    // writable() не просыпается никогда (юнит-тесты висли на CI часами).
    // На unix (level-triggered) первый же try_write отвечает, поведение
    // не меняется.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
        match stream.try_write(&[]) {
            Ok(_) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("connect к {addr} не завершился за 30 с"),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Собирает диал-функцию релеев host-бутылки: каждое исходящее
/// соединение привязывается к адаптеру `ifindex`.
fn bound_dial(binder: Arc<dyn IfaceBinder>, ifindex: u32) -> Dial {
    Arc::new(move |addr: SocketAddr| {
        let binder = Arc::clone(&binder);
        let boxed: DialFuture =
            Box::pin(async move { dial_bound(binder.as_ref(), ifindex, addr).await });
        boxed
    })
}

// ----------------------------------------------------------------------
// Windows-запрос адаптеров (парсер — фикс-юниты на эталонных фрагментах).
// ----------------------------------------------------------------------

/// Аргументы PowerShell-снапшота адаптеров: `Get-NetAdapter` текстовыми
/// колонками без заголовков (`Name InterfaceIndex Status`), сортировка
/// по имени — детерминизм парсера. `-Width 512` — страховка от усечения
/// имён `Format-Table` (как в [`crate::host_net::physical_gateway_ps`]).
pub fn adapters_ps() -> Vec<String> {
    [
        "Get-NetAdapter",
        "|",
        "Sort-Object",
        "-Property",
        "Name",
        "|",
        "Select-Object",
        "Name,",
        "InterfaceIndex,",
        "Status",
        "|",
        "Format-Table",
        "-HideTableHeaders",
        "|",
        "Out-String",
        "-Width",
        "512",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Разбирает вывод [`adapters_ps`]: строки-колонки
/// `Name InterfaceIndex Status` (разделение по whitespace; имя с
/// пробелами — всё, кроме двух последних токенов). Мусорные строки
/// (обрывки переноса, не-числовой индекс) пропускаются.
pub fn parse_adapters(text: &str) -> Vec<NetAdapter> {
    let mut out = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        let Ok(idx) = tokens[tokens.len() - 2].parse::<u32>() else {
            continue;
        };
        let status = tokens[tokens.len() - 1];
        let alias = tokens[..tokens.len() - 2].join(" ");
        out.push(NetAdapter {
            alias,
            idx,
            up: status.eq_ignore_ascii_case("Up"),
        });
    }
    out
}

// ----------------------------------------------------------------------
// Менеджер.
// ----------------------------------------------------------------------

/// Инъекционные части host-менеджера; продакшн собирает [`HostParts::real`],
/// тесты — фейки.
pub struct HostParts {
    /// Быстрая проверка наличия службы маршрутов (T7); в тестах на
    /// не-Windows переопределяется, чтобы пройти предусловие старта.
    pub svc_present: fn() -> bool,
    /// Фабрика клиентов службы маршрутов (poison-политика).
    pub svc_factory: Arc<dyn SvcFactory>,
    /// Снапшоты адаптеров (`Get-NetAdapter`).
    pub adapters: Arc<dyn AdapterSource>,
    /// Запуск VPN-клиента.
    pub spawner: Arc<dyn ClientSpawner>,
    /// Физические шлюзы ОС.
    pub gateways: Arc<dyn GatewaySource>,
    /// Резолвер endpoint'ов.
    pub resolver: Arc<dyn EndpointResolver>,
    /// Привязка исходящих сокетов релеев.
    pub binder: Arc<dyn IfaceBinder>,
}

impl HostParts {
    /// Продакшн-части. На не-Windows — стабы с отказами: host-стек
    /// (служба маршрутов, PowerShell-сеть) существует только на Windows,
    /// старт отсекается проверкой `svc_present` раньше стабов.
    pub fn real() -> Self {
        Self {
            svc_present: svc_pipe_exists,
            svc_factory: Arc::new(RealSvcFactory),
            adapters: Arc::new(RealAdapters),
            spawner: Arc::new(RealClientSpawner),
            gateways: Arc::new(RealGateways),
            resolver: Arc::new(RealEndpointResolver),
            binder: Arc::new(RealIfaceBinder),
        }
    }
}

/// Тайминги host-менеджера (сжимаются в тестах).
#[derive(Debug, Clone, Copy)]
pub struct HostTiming {
    /// Ожидание появления VPN-интерфейса.
    pub iface_timeout: Duration,
    /// Шаг поллинга адаптеров.
    pub iface_step: Duration,
}

impl Default for HostTiming {
    fn default() -> Self {
        Self {
            iface_timeout: IFACE_WAIT_TIMEOUT,
            iface_step: IFACE_POLL_STEP,
        }
    }
}

/// Сессия host-бутылки в карте менеджера.
pub struct HostRoutes {
    /// Текущее состояние.
    pub state: BottleState,
    /// Поколение маркера `starting` этого старта (гонки stop/restart,
    /// паттерн `manager::Runtime`).
    pub generation: u64,
    /// Endpoint-хосты профиля (перепроверяются DNS-циклом T10).
    pub endpoints: Vec<String>,
    /// Живое множество адресов сессии: начальный резолв + накопление
    /// DNS-циклом; убирается только stop (спека §7.2 шаг 5).
    pub addrs: Vec<IpAddr>,
    /// Выбранный адаптер VPN (после обнаружения интерфейса);
    /// информационное поле сессии (живость цикл T11 проверяет по
    /// индексу ниже).
    pub iface_alias: Option<String>,
    /// Индекс адаптера VPN: привязка релеев, исключения шлюза и
    /// живость интерфейса в health-цикле (T11, поллинг по idx).
    pub iface_idx: Option<u32>,
    /// Резерв портов релеев; `None` после teardown.
    pub listeners: Option<HostListeners>,
    /// Задачи релеев; гасятся при stop.
    pub tasks: JoinSet<()>,
}

impl HostRoutes {
    /// Пустой маркер `starting` (фаза 1 старта).
    fn marker(generation: u64, endpoints: Vec<String>) -> Self {
        Self {
            state: BottleState::Starting,
            generation,
            endpoints,
            addrs: Vec::new(),
            iface_alias: None,
            iface_idx: None,
            listeners: None,
            tasks: JoinSet::new(),
        }
    }
}

/// Результат успешного провижининга host-бутылки (фаза без лока).
struct HostProvisioned {
    endpoints: Vec<String>,
    addrs: Vec<IpAddr>,
    iface_alias: String,
    iface_idx: u32,
    listeners: HostListeners,
    tasks: JoinSet<()>,
}

/// Провал провижининга: причина + адреса, чьи маршруты надо откатить.
struct Failed {
    reason: String,
    own_addrs: Vec<IpAddr>,
}

impl Failed {
    fn reason(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            own_addrs: Vec::new(),
        }
    }

    fn with_routes(reason: impl Into<String>, own_addrs: Vec<IpAddr>) -> Self {
        Self {
            reason: reason.into(),
            own_addrs,
        }
    }
}

/// Менеджер host-бутылок: старт/стоп/статус (спека §7.2).
pub struct HostManager {
    /// Инъекционные части.
    parts: HostParts,
    /// Тайминги (сжаты в тестах).
    timing: HostTiming,
    /// Кэш клиента службы маршрутов; io-ошибка транспорта сбрасывает
    /// его (poison-политика: пересоздание через фабрику).
    svc: Mutex<Option<Arc<SvcClient>>>,
    /// Сериализация восстановления живого множества после poison:
    /// одновременные io-отказы (старт + DNS-цикл) не должны
    /// переустанавливать маршруты дважды (finding 2 ревью T9/T10).
    restore_lock: Mutex<()>,
    /// Тест-обсервер сериализации восстановления: сколько раз
    /// конкурент дожидался чужого восстановления и пропускал своё
    /// (ветка ожидания [`Self::restore_live_routes`]). Только в
    /// тест-сборке.
    #[cfg(test)]
    restore_waits: std::sync::atomic::AtomicUsize,
    /// Живые (и упавшие) host-сессии по id: живое множество endpoint'ов
    /// всех активных бутылок агрегируется отсюда (спека §7.2 шаг 2).
    endpoints_state: Mutex<HashMap<String, HostRoutes>>,
    /// Счётчик поколений маркеров `starting`.
    generations: AtomicU64,
    /// Канал смертей relay-задач host-сессий (T11, п.3): задачи релеев
    /// докладывают ошибки акцепт-цикла; health-цикл демона дренирует
    /// канал и помечает сессию `Degraded{причина}`.
    relay_tx: mpsc::UnboundedSender<RelayDeath>,
    relay_rx: Mutex<mpsc::UnboundedReceiver<RelayDeath>>,
}

impl HostManager {
    /// Создаёт менеджера из частей и таймингов.
    pub fn new(parts: HostParts, timing: HostTiming) -> Self {
        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        Self {
            parts,
            timing,
            svc: Mutex::new(None),
            restore_lock: Mutex::new(()),
            #[cfg(test)]
            restore_waits: std::sync::atomic::AtomicUsize::new(0),
            endpoints_state: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
            relay_tx,
            relay_rx: Mutex::new(relay_rx),
        }
    }

    /// Поднимает host-бутылку по спеке §7.2.
    ///
    /// Три фазы (паттерн [`crate::manager::BottleManager::start`]):
    /// (1) короткий лок — маркер `starting` со свежим поколением
    /// (активная бутылка — no-op); (2) без лока — резолв, маршруты,
    /// клиент, интерфейс, релеи; (3) короткий лок — фиксация результата,
    /// если поколение записи всё ещё наше; иначе полный откат.
    pub async fn start(&self, profile: &BottleProfile) -> Result<CliReply, DaemonError> {
        let VpnConfig::Host { endpoints, .. } = &profile.vpn else {
            return Ok(CliReply::Error {
                message: "профиль не является host-бутылкой".to_string(),
            });
        };

        // Предусловие T7: без службы маршрутов host-бутылки недоступны.
        if !(self.parts.svc_present)() {
            tracing::warn!("служба маршрутов не установлена: host-старт отклонён");
            return Ok(CliReply::Error {
                message: SVC_MISSING_MESSAGE.to_string(),
            });
        }

        // Фаза 1: маркер starting.
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = self.endpoints_state.lock().await;
            if let Some(existing) = state.get(&profile.id) {
                if host_is_active(&existing.state) {
                    tracing::warn!(
                        id = %profile.id,
                        state = existing.state.as_str(),
                        "host-бутылка уже активна: повторный start без действия"
                    );
                    return Ok(CliReply::Started {
                        id: profile.id.clone(),
                    });
                }
            }
            state.insert(
                profile.id.clone(),
                HostRoutes::marker(generation, endpoints.clone()),
            );
        }

        // Фаза 2: длинные операции без лока.
        let outcome = self.provision(profile).await;

        // Фаза 3: фиксация или откат.
        let mut state = self.endpoints_state.lock().await;
        let marker_alive = state
            .get(&profile.id)
            .is_some_and(|r| r.generation == generation);
        match outcome {
            Ok(made) if marker_alive => {
                state.insert(
                    profile.id.clone(),
                    HostRoutes {
                        state: BottleState::Up,
                        generation,
                        endpoints: made.endpoints,
                        addrs: made.addrs,
                        iface_alias: Some(made.iface_alias),
                        iface_idx: Some(made.iface_idx),
                        listeners: Some(made.listeners),
                        tasks: made.tasks,
                    },
                );
                drop(state);
                tracing::info!(id = %profile.id, "host-бутылка поднята");
                if let Some(command) = &profile.expose.launch {
                    spawn_launch(command);
                }
                Ok(CliReply::Started {
                    id: profile.id.clone(),
                })
            }
            Ok(mut made) => {
                // Гонку выиграл stop (записи нет) или рестарт (чужое
                // поколение): разобрать всё сделанное этим запуском.
                drop(state);
                made.tasks.abort_all();
                while made.tasks.join_next().await.is_some() {}
                drop(made.listeners);
                self.del_session_routes(&made.addrs).await;
                tracing::warn!(id = %profile.id, "запуск host-бутылки отменён остановкой");
                Ok(CliReply::Error {
                    message: "бутылка остановлена во время запуска".to_string(),
                })
            }
            Err(reason) => {
                if marker_alive {
                    state.insert(
                        profile.id.clone(),
                        HostRoutes {
                            state: BottleState::Failed {
                                reason: reason.clone(),
                            },
                            generation,
                            endpoints: endpoints.clone(),
                            addrs: Vec::new(),
                            iface_alias: None,
                            iface_idx: None,
                            listeners: None,
                            tasks: JoinSet::new(),
                        },
                    );
                }
                drop(state);
                tracing::warn!(id = %profile.id, %reason, "поднятие host-бутылки не удалось");
                // Спавн-процесс (если клиент уже запущен) не завершаем —
                // нет API (спека §7.2, шаг 6).
                Ok(CliReply::Error { message: reason })
            }
        }
    }

    /// Опускает host-бутылку (спека §7.2, шаг 6): релеи вниз, svc.del
    /// всех адресов сессии (включая накопленные DNS-циклом); клиент
    /// остаётся работать — предупреждение в ответе CLI.
    pub async fn stop(&self, id: &str) -> Result<CliReply, DaemonError> {
        // Короткий лок: активную сессию вынуть целиком.
        let removed = {
            let mut state = self.endpoints_state.lock().await;
            if state.get(id).is_some_and(|r| host_is_active(&r.state)) {
                state.remove(id)
            } else {
                None
            }
        };
        let Some(mut session) = removed else {
            // Повторный stop уже остановленной — no-op с
            // предупреждением, не ошибка (спека §5.2).
            return Ok(CliReply::Stopped {
                id: id.to_string(),
                warning: Some("не была запущена".to_string()),
            });
        };

        session.tasks.abort_all();
        while session.tasks.join_next().await.is_some() {}
        session.listeners = None;
        // Сессия уже вынута из карты — del_session_routes видит только
        // чужие живые сессии и не снимает их адреса.
        self.del_session_routes(&session.addrs).await;
        tracing::warn!(id, "клиент VPN оставлен запущенным (нет API завершения)");
        Ok(CliReply::Stopped {
            id: id.to_string(),
            warning: Some(HOST_STOP_CLIENT_WARNING.to_string()),
        })
    }

    /// Есть ли активная (`starting`/`up`/`degraded`/`stopping`) сессия
    /// `id`. Маршрутизация `stop` менеджера проверяет её ДО загрузки
    /// профиля: сессия могла остаться при удалённом/битом профиле и всё
    /// равно подлежит разбору (T11 carried minor из T9/T10).
    pub async fn has_active_session(&self, id: &str) -> bool {
        self.endpoints_state
            .lock()
            .await
            .get(id)
            .is_some_and(|r| host_is_active(&r.state))
    }

    /// Один проход живости host-интерфейсов (T11, health-цикл демона,
    /// рулинг префлайта: проверка живости СВОЕГО интерфейса, а не слепой
    /// stop по агенту). Сессии `up`/`degraded` сверяют адаптер из
    /// снапшота `Get-NetAdapter` по индексу: живой (`Up`) — не трогаем;
    /// исчез — полный host-stop путь (релеи вниз, svc.del всех адресов
    /// сессии) и метка `stopped`: VPN-клиент мёртв вместе с интерфейсом,
    /// нужен человек для его перезапуска (спека §9).
    ///
    /// Недоступный снапшот адаптеров — `warn`, сессии не трогаем:
    /// разовый сбой PowerShell не должен останавливать живые бутылки.
    pub async fn check_iface_alive(&self) {
        let watched: Vec<(String, u32)> = {
            let state = self.endpoints_state.lock().await;
            state
                .iter()
                .filter(|(_, r)| {
                    matches!(r.state, BottleState::Up | BottleState::Degraded { .. })
                        && r.iface_idx.is_some()
                })
                .filter_map(|(id, r)| r.iface_idx.map(|idx| (id.clone(), idx)))
                .collect()
        };
        if watched.is_empty() {
            return;
        }
        let adapters = match self.parts.adapters.list_adapters().await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(%error, "снапшот адаптеров не получен: живость host-интерфейсов не проверена");
                return;
            }
        };
        for (id, idx) in watched {
            if adapters.iter().any(|a| a.idx == idx && a.up) {
                continue;
            }
            // Умер: полный host-stop путь (запись вынимается — Routes
            // больше не владеет endpoint'ами), затем метка stopped.
            let removed = {
                let mut state = self.endpoints_state.lock().await;
                if state.get(&id).is_some_and(|r| host_is_active(&r.state)) {
                    state.remove(&id)
                } else {
                    None
                }
            };
            let Some(mut session) = removed else {
                continue; // сессию остановили, пока снимок добирался
            };
            session.tasks.abort_all();
            while session.tasks.join_next().await.is_some() {}
            session.listeners = None;
            self.del_session_routes(&session.addrs).await;
            let endpoints = std::mem::take(&mut session.endpoints);
            let iface_alias = session.iface_alias.take();
            let iface_idx = session.iface_idx.take();
            self.endpoints_state.lock().await.insert(
                id.clone(),
                HostRoutes {
                    state: BottleState::Stopped,
                    generation: session.generation,
                    endpoints,
                    addrs: Vec::new(),
                    iface_alias,
                    iface_idx,
                    listeners: None,
                    tasks: JoinSet::new(),
                },
            );
            tracing::warn!(
                id = %id,
                iface_index = idx,
                "VPN-интерфейс host-бутылки исчез: сессия остановлена (нужен человек для клиента)"
            );
        }
    }

    /// Разбирает накопленные смерти relay-задач host-сессий: живая
    /// (`up`/`degraded`) сессия получает `Degraded{причина}` — не
    /// `failed`: туннель/маршруты живут, умер локальный релей (T11, п.3).
    pub async fn collect_relay_deaths(&self) {
        let deaths: Vec<RelayDeath> = {
            let mut rx = self.relay_rx.lock().await;
            let mut drained = Vec::new();
            while let Ok(death) = rx.try_recv() {
                drained.push(death);
            }
            drained
        };
        for death in deaths {
            let mut state = self.endpoints_state.lock().await;
            match state.get_mut(&death.id) {
                Some(routes)
                    if matches!(routes.state, BottleState::Up | BottleState::Degraded { .. }) =>
                {
                    routes.state = BottleState::Degraded {
                        reason: death.reason.clone(),
                    };
                    tracing::warn!(id = %death.id, reason = %death.reason, "relay-задача host-сессии погибла: деградация");
                }
                _ => {}
            }
        }
    }

    /// Доклад о смерти relay-задачи сессии — тот же вызов, что делает
    /// задача релея при ошибке акцепт-цикла ([`Self::relay_tx`]).
    /// Только тесты: гибель настоящего релея воспроизвести кросс-
    /// платформенно нельзя (акцепт-цикл падает лишь на сбое слушателя).
    #[cfg(test)]
    pub(crate) fn report_relay_death_for_test(&self, id: &str, reason: &str) {
        let _ = self.relay_tx.send(RelayDeath {
            id: id.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Один проход фоновой DNS-перепроверки (T10, спека §7.2 шаг 5):
    /// endpoint'ы активных (`up`) сессий re-resolve'ятся; адреса, которых
    /// нет в живом множестве сессии, получают маршруты и пополняют его;
    /// прежние остаются до stop (не рвать установленное соединение).
    /// Ошибки резолва и установки — `warn`, не fatal (следующий проход
    /// через [`DNS_RECHECK_STEP`] повторит попытку).
    ///
    /// Вызывается фоновым циклом демона
    /// (`BottleManager::spawn_host_dns_recheck`); один проход —
    /// отдельный метод для таймлайн-тестов на фейковом резолвере.
    pub async fn recheck_dns(&self) {
        // Снимок up-сессий: starting ещё не владеет маршрутами (своим
        // ходом их ставит provisioning), stopped/failed уже не владеет.
        let sessions: Vec<(String, Vec<String>)> = {
            let state = self.endpoints_state.lock().await;
            state
                .iter()
                .filter(|(_, r)| matches!(r.state, BottleState::Up))
                .map(|(id, r)| (id.clone(), r.endpoints.clone()))
                .collect()
        };
        for (id, endpoints) in sessions {
            let resolved = match self.parts.resolver.resolve(&endpoints).await {
                Ok(addrs) => addrs,
                Err(error) => {
                    tracing::warn!(id = %id, %error, "перепроверка DNS endpoint'ов не удалась");
                    continue;
                }
            };
            // Новые адреса сессии:resolved минус её живое множество.
            let fresh: Vec<IpAddr> = {
                let state = self.endpoints_state.lock().await;
                match state.get(&id) {
                    Some(routes) => resolved
                        .into_iter()
                        .filter(|addr| !routes.addrs.contains(addr))
                        .collect(),
                    None => Vec::new(), // сессию остановили между снимком и сейчас
                }
            };
            if fresh.is_empty() {
                continue;
            }
            // Маршруты новых адресов: шлюз своего семейства (исключая
            // адаптеры активных host-сессий); недоступный шлюз или отказ
            // установки — warn, адрес останется для следующего прохода.
            let mut installed: Vec<IpAddr> = Vec::new();
            for addr in &fresh {
                let family = if addr.is_ipv4() {
                    GwFamily::V4
                } else {
                    GwFamily::V6
                };
                match self.detect_gateway(family).await {
                    Ok((gateway, alias)) => match self.svc_add(*addr, gateway, &alias).await {
                        Ok(()) => installed.push(*addr),
                        Err(error) => {
                            tracing::warn!(id = %id, %addr, %error, "маршрут нового адреса не установлен")
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            id = %id,
                            %addr,
                            %error,
                            "физический шлюз семейства недоступен — адрес пропущен до следующего прохода"
                        );
                    }
                }
            }
            // Фиксация под коротким локом; сессию могли остановить, пока
            // резолвировали и ставили маршруты, — откатить своё (адреса,
            // которыми владеют другие активные сессии, del_session_routes
            // не тронет).
            let mut state = self.endpoints_state.lock().await;
            match state.get_mut(&id) {
                Some(routes) if matches!(routes.state, BottleState::Up) => {
                    for addr in installed {
                        if !routes.addrs.contains(&addr) {
                            routes.addrs.push(addr);
                        }
                    }
                }
                _ => {
                    drop(state);
                    tracing::warn!(
                        id = %id,
                        "сессия остановлена во время перепроверки — новые маршруты откатываются"
                    );
                    self.del_session_routes(&installed).await;
                }
            }
        }
    }

    /// Рантайм-состояния host-бутылок (для `status` менеджера).
    pub async fn status(&self) -> Vec<RuntimeState> {
        let state = self.endpoints_state.lock().await;
        state
            .iter()
            .map(|(id, r)| {
                let (socks5, forwards) = match &r.listeners {
                    Some(listeners) => (
                        listeners.socks5.local_addr().ok().map(|addr| addr.port()),
                        listeners.forward_specs(),
                    ),
                    None => (None, Vec::new()),
                };
                RuntimeState {
                    id: id.clone(),
                    state: r.state.clone(),
                    socks5,
                    forwards,
                }
            })
            .collect()
    }

    /// Фаза 2: маршруты → снапшот → клиент → интерфейс → релеи.
    /// Ошибка после установки маршрутов снимает их (полный откат).
    async fn provision(&self, profile: &BottleProfile) -> Result<HostProvisioned, String> {
        match self.provision_steps(profile).await {
            Ok(made) => Ok(made),
            Err(failed) => {
                if !failed.own_addrs.is_empty() {
                    tracing::warn!(id = %profile.id, "откат маршрутов сессии после неудачи");
                    self.del_session_routes(&failed.own_addrs).await;
                }
                Err(failed.reason)
            }
        }
    }

    /// Шаги провижининга; провал после установки маршрутов возвращает их
    /// список для отката.
    async fn provision_steps(&self, profile: &BottleProfile) -> Result<HostProvisioned, Failed> {
        let VpnConfig::Host {
            client,
            args,
            endpoints,
            interface,
        } = &profile.vpn
        else {
            return Err(Failed::reason("профиль не является host-бутылкой"));
        };

        // 1. Резолв endpoint'ов своей сессии (дедуп с сохранением порядка).
        let resolved = self
            .parts
            .resolver
            .resolve(endpoints)
            .await
            .map_err(|e| Failed::reason(e.to_string()))?;
        let mut addrs: Vec<IpAddr> = Vec::new();
        for addr in resolved {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        let has_v4 = addrs.iter().any(IpAddr::is_ipv4);
        let has_v6 = addrs.iter().any(IpAddr::is_ipv6);

        // 2. Спека §9: v6-endpoint'ы без v6-шлюза физики. Блокируем
        // запуск, только если НИ ОДИН endpoint не имеет v4-адреса; при
        // живом v4 продолжаем только по v4 (v6-адреса без шлюза не
        // маршрутизируются).
        let use_v6 = if has_v6 {
            match self.detect_gateway(GwFamily::V6).await {
                Ok(_) => true,
                Err(_) if !has_v4 => {
                    return Err(Failed::reason(format!(
                        "endpoint {} имеет IPv6, а у физического интерфейса нет IPv6-шлюза — \
                         запуск заблокирован (спека §9)",
                        endpoints.join(", ")
                    )));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "v6-шлюз недоступен: маршруты endpoint'ов только по v4 (спека §9)"
                    );
                    false
                }
            }
        } else {
            false
        };
        let own_routable: Vec<IpAddr> = addrs
            .iter()
            .copied()
            .filter(|addr| use_v6 || addr.is_ipv4())
            .collect();

        // 3. Множество активных host-endpoint'ов: чужие сессии + своя
        // стартующая (спека §7.2 шаг 2 — маршруты чужих бутылок обязаны
        // стоять до запуска клиента).
        let union = self.union_with_active(&profile.id, &own_routable).await;

        // 4. Шлюзы по семействам присутствующих в объединении адресов.
        // Семейство без шлюза — жёсткая ошибка, если он нужен своим
        // адресам (отфильтрованным шагом 2); чужим — warn и пропуск:
        // их маршруты могла поставить своя сессия раньше, а старт
        // бутылки не должен валиться из-за чужих v6-адресов при
        // пропавшем v6-шлюзе физики.
        let pick_gateway = |need_own: bool, family: GwFamily| async move {
            match self.detect_gateway(family).await {
                Ok(found) => Ok(Some(found)),
                Err(error) if need_own => Err(Failed::reason(format!(
                    "физический шлюз семейства {family:?} не найден: {error}"
                ))),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        family = ?family,
                        "шлюз недоступен: endpoint'ы чужих сессий этого семейства пропущены"
                    );
                    Ok(None)
                }
            }
        };
        let own_has_v4 = own_routable.iter().any(IpAddr::is_ipv4);
        let own_has_v6 = own_routable.iter().any(IpAddr::is_ipv6);
        let gw4 = if union.iter().any(IpAddr::is_ipv4) {
            pick_gateway(own_has_v4, GwFamily::V4).await?
        } else {
            None
        };
        let gw6 = if union.iter().any(IpAddr::is_ipv6) {
            pick_gateway(own_has_v6, GwFamily::V6).await?
        } else {
            None
        };

        // 5. Pin-маршруты всего множества ДО спавна клиента; адреса
        // семейства без шлюза (чужие, шаг 4) пропускаются с warn.
        for addr in &union {
            let Some((gateway, alias)) = (match addr {
                IpAddr::V4(_) => gw4.as_ref(),
                IpAddr::V6(_) => gw6.as_ref(),
            }) else {
                tracing::warn!(%addr, "маршрут адреса не установлен: шлюз семейства недоступен");
                continue;
            };
            if let Err(error) = self.svc_add(*addr, *gateway, alias).await {
                return Err(Failed::with_routes(
                    format!("не удалось установить маршрут endpoint'а: {error}"),
                    own_routable.clone(),
                ));
            }
        }

        // 6. Снапшот адаптеров ДО запуска клиента.
        let before = self.parts.adapters.list_adapters().await.map_err(|e| {
            Failed::with_routes(
                format!("снапшот адаптеров не получен: {e}"),
                own_routable.clone(),
            )
        })?;

        // 7. Запуск клиента (мини-shlex: кавычки и экранированные пробелы
        // в аргументах, см. `launch::split_command_line`).
        let argv = split_command_line(args.as_deref().unwrap_or(""));
        if let Err(error) = self.parts.spawner.spawn(client, &argv).await {
            return Err(Failed::with_routes(
                format!("не удалось запустить VPN-клиент {client}: {error}"),
                own_routable.clone(),
            ));
        }

        // 8. Ожидание VPN-интерфейса.
        let iface = match self.wait_iface(&before, interface.as_deref()).await {
            Ok(iface) => iface,
            Err(reason) => {
                return Err(Failed::with_routes(reason, own_routable.clone()));
            }
        };

        // 9. Релеи: резерв портов + задачи с привязкой исходящих сокетов.
        let selection = reserve_ports(profile)
            .map_err(|reason| Failed::with_routes(reason, own_routable.clone()))?;
        let mut tasks = JoinSet::new();
        if let Err(reason) = spawn_host_relays(
            &mut tasks,
            &selection,
            Arc::clone(&self.parts.binder),
            iface.idx,
            &profile.id,
            self.relay_tx.clone(),
        ) {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            drop(selection);
            return Err(Failed::with_routes(reason, own_routable));
        }

        Ok(HostProvisioned {
            endpoints: endpoints.clone(),
            addrs: own_routable,
            iface_alias: iface.alias,
            iface_idx: iface.idx,
            listeners: selection,
            tasks,
        })
    }

    /// Поллинг появления VPN-интерфейса (спека §7.2, шаг 3).
    ///
    /// Явный `wanted` — ждём адаптер с этим именем в состоянии `Up`
    /// (кейс «тот же вендор-клиент уже запущен»: существующий интерфейс
    /// переиспользуется; сравнение кейс-инсенситивно). Автоопределение —
    /// диф-снапшот: ровно один новый `Up` берёмся, несколько — ошибка
    /// с просьбой указать `interface`, ноль — таймаут.
    async fn wait_iface(
        &self,
        before: &[NetAdapter],
        wanted: Option<&str>,
    ) -> Result<NetAdapter, String> {
        let deadline = tokio::time::Instant::now() + self.timing.iface_timeout;
        loop {
            let list = match self.parts.adapters.list_adapters().await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!(%error, "снапшот адаптеров не получен, повтор поллинга");
                    Vec::new()
                }
            };
            if let Some(name) = wanted {
                if let Some(found) = list
                    .iter()
                    .find(|a| a.up && a.alias.eq_ignore_ascii_case(name))
                {
                    tracing::info!(iface = %found.alias, "VPN-интерфейс найден по имени профиля");
                    return Ok(found.clone());
                }
            } else {
                let new_ups: Vec<&NetAdapter> = list
                    .iter()
                    .filter(|a| a.up && !before.iter().any(|b| b.idx == a.idx))
                    .collect();
                if new_ups.len() == 1 {
                    tracing::info!(iface = %new_ups[0].alias, "VPN-интерфейс найден автоопределением");
                    return Ok(new_ups[0].clone());
                }
                if new_ups.len() > 1 {
                    let names: Vec<&str> = new_ups.iter().map(|a| a.alias.as_str()).collect();
                    return Err(format!(
                        "появилось несколько интерфейсов ({}) — укажите interface в профиле",
                        names.join(", ")
                    ));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "интерфейс не появился за {:?}",
                    self.timing.iface_timeout
                ));
            }
            tokio::time::sleep(self.timing.iface_step).await;
        }
    }

    /// Живое множество активных endpoint'ов: адреса чужих активных сессий
    /// + свои маршрутизируемые адреса (дедуп, порядок стабильный).
    async fn union_with_active(&self, own_id: &str, own: &[IpAddr]) -> Vec<IpAddr> {
        let state = self.endpoints_state.lock().await;
        let mut union: Vec<IpAddr> = Vec::new();
        for (id, routes) in state.iter() {
            if id != own_id && host_is_active(&routes.state) {
                for addr in &routes.addrs {
                    if !union.contains(addr) {
                        union.push(*addr);
                    }
                }
            }
        }
        for addr in own {
            if !union.contains(addr) {
                union.push(*addr);
            }
        }
        union
    }

    /// Физический шлюз семейства с исключением адаптеров активных
    /// host-сессий (их default-маршруты — VPN, не физика).
    async fn detect_gateway(&self, family: GwFamily) -> Result<(IpAddr, String), String> {
        let exclude: Vec<u32> = {
            let state = self.endpoints_state.lock().await;
            state
                .values()
                .filter(|r| host_is_active(&r.state))
                .filter_map(|r| r.iface_idx)
                .collect()
        };
        self.parts
            .gateways
            .gateway(&exclude, family)
            .await
            .map_err(|error| error.to_string())
    }

    /// svc.add с poison-политикой (пересоздание клиента при io-ошибке).
    async fn svc_add(
        &self,
        dest: IpAddr,
        gateway: IpAddr,
        iface_alias: &str,
    ) -> Result<(), String> {
        self.try_svc("add", |client| async move {
            client.add(dest, gateway, iface_alias).await
        })
        .await
        .map_err(|e| e.to_string())
    }

    /// svc.add без каскада восстановления (сам используется
    /// восстановлением после пересоздания клиента).
    async fn svc_add_no_restore(
        &self,
        dest: IpAddr,
        gateway: IpAddr,
        iface_alias: &str,
    ) -> Result<(), String> {
        self.try_svc_checked(
            "add",
            |client| async move { client.add(dest, gateway, iface_alias).await },
            false,
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// svc.del с poison-политикой; ошибка снятия — warn, не fatal
    /// (остаточные маршруты убирает сверка журнала службы, спека §9).
    async fn svc_del(&self, dest: IpAddr) {
        if let Err(error) = self
            .try_svc("del", |client| async move { client.del(dest).await })
            .await
        {
            tracing::warn!(%dest, %error, "не удалось снять маршрут endpoint'а");
        }
    }

    /// Выполняет операцию службы маршрутов; при io-ошибке транспорта
    /// пересоздаёт клиент (poison-политика T7/T9) и повторяет один раз.
    async fn try_svc<F, Fut>(&self, op: &str, operation: F) -> Result<(), DaemonError>
    where
        // `Fn`, не `FnOnce`: повтор Poison-политики вызывает замыкание
        // второй раз (все захваченные значения — Copy).
        F: Fn(Arc<SvcClient>) -> Fut,
        Fut: std::future::Future<Output = Result<(), DaemonError>>,
    {
        self.try_svc_checked(op, operation, true).await
    }

    /// [`try_svc`] с флагом каскада восстановления (см.
    /// [`HostManager::restore_live_routes`]).
    async fn try_svc_checked<F, Fut>(
        &self,
        op: &str,
        operation: F,
        restore: bool,
    ) -> Result<(), DaemonError>
    where
        F: Fn(Arc<SvcClient>) -> Fut,
        Fut: std::future::Future<Output = Result<(), DaemonError>>,
    {
        for attempt in 0..2 {
            let client = self.svc_client().await?;
            match operation(client).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt == 0 && matches!(error, DaemonError::Io(_)) => {
                    tracing::warn!(
                        op,
                        %error,
                        "транспорт службы маршрутов отказал — клиент пересоздаётся (poison)"
                    );
                    *self.svc.lock().await = None;
                    // Разрыв соединения закрывает сессию службы, а с ней
                    // — ВСЕ маршруты демона (спека §9): новый клиент
                    // получает новую сессию, живое множество активных
                    // бутылок переустанавливается. Box::pin — разрыв
                    // рекурсии типов (восстановление само ходит через
                    // try_svc_checked без каскада).
                    if restore {
                        Box::pin(self.restore_live_routes()).await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("вторая итерация цикла всегда возвращается")
    }

    /// Кэшированный клиент службы маршрутов (создаётся при отсутствии).
    async fn svc_client(&self) -> Result<Arc<SvcClient>, DaemonError> {
        let mut cached = self.svc.lock().await;
        if let Some(client) = cached.as_ref() {
            return Ok(Arc::clone(client));
        }
        let client = self.parts.svc_factory.create().await?;
        *cached = Some(Arc::clone(&client));
        Ok(client)
    }

    /// Переустанавливает живое множество endpoint'ов активных сессий
    /// после пересоздания клиента службы: старая сессия умерла вместе
    /// со старым соединением, и служба сняла её маршруты (спека §9) —
    /// без восстановления активные бутылки молча теряли бы pin-маршруты.
    /// Ошибки отдельных адресов — warn (следующая операция/проход
    /// DNS-цикла повторит); рекурсии нет ([`Self::svc_add_no_restore`]).
    ///
    /// Сериализация (finding 2 ревью): одновременные poison-события
    /// (старт бутылки + DNS-цикл) восстанавливают множество один раз —
    /// конкурент, заставший чужое восстановление, дожидается его и
    /// пропускает своё (дубликаты `New-NetRoute` не нужны).
    async fn restore_live_routes(&self) {
        // Держатель гварда восстанавливает; ожидавшие — нет.
        let Ok(_guard) = self.restore_lock.try_lock() else {
            // Ждём завершения чужого восстановления; гвард отпускаем
            // сразу — своё пропускаем (дублей New-NetRoute не нужно).
            #[cfg(test)]
            self.restore_waits.fetch_add(1, Ordering::SeqCst);
            drop(self.restore_lock.lock().await);
            return;
        };
        let union: Vec<IpAddr> = {
            let state = self.endpoints_state.lock().await;
            let mut union: Vec<IpAddr> = Vec::new();
            for routes in state.values().filter(|r| host_is_active(&r.state)) {
                for addr in &routes.addrs {
                    if !union.contains(addr) {
                        union.push(*addr);
                    }
                }
            }
            union
        };
        if union.is_empty() {
            return;
        }
        tracing::warn!(
            count = union.len(),
            "сессия службы маршрутов пересоздана: восстановление pin-маршрутов активных бутылок"
        );
        for addr in union {
            let family = if addr.is_ipv4() {
                GwFamily::V4
            } else {
                GwFamily::V6
            };
            match self.detect_gateway(family).await {
                Ok((gateway, alias)) => {
                    if let Err(error) = self.svc_add_no_restore(addr, gateway, &alias).await {
                        tracing::warn!(%addr, %error, "восстановление маршрута не удалось");
                    }
                }
                Err(error) => {
                    tracing::warn!(%addr, %error, "шлюз семейства недоступен при восстановлении")
                }
            }
        }
    }

    /// Снимает маршруты адресов сессии, если ими не владеет ни одна
    /// другая активная сессия (общий endpoint двух бутылок остаётся
    /// маршрутизированным для живой).
    async fn del_session_routes(&self, addrs: &[IpAddr]) {
        let held_by_others: Vec<IpAddr> = {
            let state = self.endpoints_state.lock().await;
            state
                .values()
                .filter(|r| host_is_active(&r.state))
                .flat_map(|r| r.addrs.iter().copied())
                .collect()
        };
        for addr in addrs {
            if held_by_others.contains(addr) {
                tracing::debug!(%addr, "адрес endpoint'а у другой сессии — её маршрут живёт");
                continue;
            }
            self.svc_del(*addr).await;
        }
    }
}

/// Активные состояния host-сессий (зеркало `manager::is_active`).
fn host_is_active(state: &BottleState) -> bool {
    matches!(
        state,
        BottleState::Starting
            | BottleState::Up
            | BottleState::Degraded { .. }
            | BottleState::Stopping
    )
}

/// Поднимает задачи релеев host-бутылки: SOCKS5 + пробросы, исходящие
/// соединения — через [`dial_bound`] с привязкой к адаптеру VPN. Ошибки
/// акцепт-цикла (смерть задачи, не по остановке) докладываются в
/// `reporter` — health-цикл пометит сессию `Degraded` (T11, п.3).
fn spawn_host_relays(
    tasks: &mut JoinSet<()>,
    selection: &HostListeners,
    binder: Arc<dyn IfaceBinder>,
    ifindex: u32,
    id: &str,
    reporter: mpsc::UnboundedSender<RelayDeath>,
) -> Result<(), String> {
    let dial = bound_dial(Arc::clone(&binder), ifindex);
    let id = id.to_string();

    let socks5_port = selection
        .socks5_port()
        .map_err(|e| format!("релей SOCKS5: {e}"))?;
    let socks5_dup = selection
        .socks5
        .try_clone()
        .and_then(|dup| tokio::net::TcpListener::from_std(dup))
        .map_err(|e| format!("не удалось клонировать слушателя SOCKS5: {e}"))?;
    let socks5_dial = Arc::clone(&dial);
    let socks5_id = id.clone();
    let socks5_reporter = reporter.clone();
    tasks.spawn(async move {
        if let Err(error) = serve_socks5_with_dial(socks5_dup, socks5_dial).await {
            tracing::warn!(port = socks5_port, %error, "сервер SOCKS5 остановлен");
            let _ = socks5_reporter.send(RelayDeath {
                id: socks5_id,
                reason: format!("релей SOCKS5 127.0.0.1:{socks5_port} остановлен: {error}"),
            });
        }
    });

    for (spec, listener) in &selection.forwards {
        let target: TargetAddr = spec
            .target
            .parse()
            .map_err(|e| format!("цель проброса {:?}: {e}", spec.target))?;
        let dup = listener
            .try_clone()
            .and_then(|dup| tokio::net::TcpListener::from_std(dup))
            .map_err(|e| format!("не удалось клонировать слушателя порта {}: {e}", spec.port))?;
        let port = spec.port;
        let forward_dial = Arc::clone(&dial);
        let forward_id = id.clone();
        let forward_reporter = reporter.clone();
        tasks.spawn(async move {
            if let Err(error) = serve_forward_with_dial(dup, target, forward_dial).await {
                tracing::warn!(port, %error, "сервер проброса остановлен");
                let _ = forward_reporter.send(RelayDeath {
                    id: forward_id,
                    reason: format!("релей 127.0.0.1:{port} остановлен: {error}"),
                });
            }
        });
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Реальные Windows-части.
// ----------------------------------------------------------------------

/// Продакшн-фабрика клиентов службы: named pipe + `Hello`
/// ([`crate::svc_client::RealSvcPipe`]).
pub struct RealSvcFactory;

#[async_trait]
impl SvcFactory for RealSvcFactory {
    async fn create(&self) -> Result<Arc<SvcClient>, DaemonError> {
        #[cfg(windows)]
        {
            let transport = crate::svc_client::RealSvcPipe::connect().await?;
            Ok(Arc::new(SvcClient::connect(Arc::new(transport)).await?))
        }
        #[cfg(not(windows))]
        {
            Err(DaemonError::Svc(
                "служба маршрутов доступна только на Windows".to_string(),
            ))
        }
    }
}

/// Продакшн-снапшоты адаптеров: `Get-NetAdapter` (Windows); не-Windows —
/// отказ (host-стек демона Windows-only).
pub struct RealAdapters;

#[async_trait]
impl AdapterSource for RealAdapters {
    async fn list_adapters(&self) -> io::Result<Vec<NetAdapter>> {
        #[cfg(windows)]
        {
            let output = crate::ps_query::run_ps_readonly(&adapters_ps()).await?;
            Ok(parse_adapters(&output))
        }
        #[cfg(not(windows))]
        {
            Err(io::Error::other(
                "список адаптеров доступен только на Windows (Get-NetAdapter)",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec};
    use bottle_core::svcrpc::{SvcReply, SvcRequest};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::error::DaemonError;
    use crate::manager::BottleManager;
    use crate::store::Store;
    use crate::svc_client::SvcTransport;

    // ------------------------------------------------------------------
    // Фейки.
    // ------------------------------------------------------------------

    /// Общий журнал порядка операций (svc/spawn/bind/accept) — пины
    /// «маршруты до спавна», «привязка до connect».
    #[derive(Clone, Default)]
    struct Ops(Arc<StdMutex<Vec<String>>>);

    impl Ops {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().unwrap().push(entry.into());
        }

        fn snapshot(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        /// Индекс первого вхождения подстроки.
        fn first(&self, needle: &str) -> usize {
            self.snapshot()
                .iter()
                .position(|e| e.contains(needle))
                .unwrap_or_else(|| panic!("в журнале нет {needle:?}: {:?}", self.snapshot()))
        }

        fn count(&self, needle: &str) -> usize {
            self.snapshot()
                .iter()
                .filter(|e| e.contains(needle))
                .count()
        }
    }

    /// Рандеву на первом Add транспорта: первый Add паркуется, следующий
    /// Add на том же транспорте его будит и проходит сам. Загоняет
    /// восстановление живого множества (держатель гварда) внутрь чужого
    /// Add — конкурентный poison застаёт гвард занятым и уходит в ветку
    /// ожидания. Детерминированно на current_thread-runtime (`join!`
    /// опрашивает ветки по порядку, парковка — единственная точка
    /// передачи управления).
    #[derive(Default)]
    struct AddGate {
        /// 0 — взведён, прибытий нет; 1 — первый Add паркуется;
        /// 2 — гейт отработал.
        state: StdMutex<u32>,
        /// Будит парковку первого Add.
        notify: tokio::sync::Notify,
    }

    impl AddGate {
        /// Первый Add на транспорте ждёт следующего; следующий будит
        /// его и идёт дальше сразу.
        async fn arrive(&self) {
            let first = {
                let mut state = self.state.lock().unwrap();
                *state += 1;
                *state == 1
            };
            if first {
                self.notify.notified().await;
            } else {
                self.notify.notify_one();
            }
        }
    }

    /// Фейк-транспорт службы: всё `Ok`, журнал запросов; очередные
    /// `Add` могут упасть io-ошибкой (poison-политика — клиент
    /// пересоздаётся). Счётчик отказов общий с фабрикой: тест
    /// «взводит» его на живом кэшированном транспорте. Необязательный
    /// [`AddGate`] паркует первый Add транспорта (хореография
    /// двойного poison).
    struct FakeSvc {
        ops: Ops,
        fail_adds_left: Arc<StdMutex<u32>>,
        add_gate: Option<Arc<AddGate>>,
    }

    #[async_trait]
    impl SvcTransport for FakeSvc {
        async fn request(&self, req: &SvcRequest) -> io::Result<SvcReply> {
            match req {
                SvcRequest::Hello { .. } => self.ops.push("svc:Hello"),
                SvcRequest::Add { dest, .. } => {
                    if let Some(gate) = &self.add_gate {
                        gate.arrive().await;
                    }
                    let mut fail = self.fail_adds_left.lock().unwrap();
                    if *fail > 0 {
                        *fail -= 1;
                        self.ops.push(format!("svc:Add({dest})~io"));
                        return Err(io::Error::other("фейк: транспорт службы отказал"));
                    }
                    self.ops.push(format!("svc:Add({dest})"));
                }
                SvcRequest::Del { dest } => self.ops.push(format!("svc:Del({dest})")),
                SvcRequest::Cleanup => self.ops.push("svc:Cleanup"),
                SvcRequest::List => self.ops.push("svc:List"),
            }
            Ok(SvcReply::Ok)
        }
    }

    /// Фабрика фейк-клиентов (новое соединение = новый `Hello`);
    /// счётчик «следующие N Add падают io» общий со всеми созданными
    /// транспортами.
    struct FakeSvcFactory {
        ops: Ops,
        fail_adds_left: Arc<StdMutex<u32>>,
        /// Одноразовый гейт для СЛЕДУЮЩЕГО созданного транспорта
        /// (хореография двойного poison; см. [`AddGate`]).
        gate_next: StdMutex<Option<Arc<AddGate>>>,
    }

    impl FakeSvcFactory {
        /// Следующий Add на любом живом транспорте уронит io
        /// (arm poison-карусели посреди теста — в т.ч. на кэшированном
        /// клиенте).
        fn fail_next_add(&self) {
            self.fail_next_adds(1);
        }

        /// Следующие `n` Add на любых живых транспортах уронят io
        /// (параллельные poison-события).
        fn fail_next_adds(&self, n: u32) {
            *self.fail_adds_left.lock().unwrap() = n;
        }

        /// Первый Add СЛЕДУЮЩЕГО созданного транспорта паркуется до
        /// второго Add на нём (заставляет конкурентные poison-события
        /// перекрыться: держатель гварда восстановления ждёт внутри
        /// своего Add).
        fn gate_next_add(&self) {
            *self.gate_next.lock().unwrap() = Some(Arc::new(AddGate::default()));
        }
    }

    #[async_trait]
    impl SvcFactory for FakeSvcFactory {
        async fn create(&self) -> Result<Arc<SvcClient>, DaemonError> {
            let add_gate = self.gate_next.lock().unwrap().take();
            SvcClient::connect(Arc::new(FakeSvc {
                ops: self.ops.clone(),
                fail_adds_left: Arc::clone(&self.fail_adds_left),
                add_gate,
            }))
            .await
            .map(Arc::new)
        }
    }

    /// Ответ фейк-резолвера: адреса или отказ (DNS-цикл T10 проверяет
    /// оба пути).
    enum FakeAnswer {
        /// Резолв успешен.
        Addrs(Vec<IpAddr>),
        /// Резолв отказал (warn-путь DNS-цикла).
        Fail(String),
    }

    /// Фейк-резолвер: текущий ответ, меняемый тестом.
    struct FakeResolver(StdMutex<FakeAnswer>);

    impl FakeResolver {
        fn set(&self, addrs: Vec<IpAddr>) {
            *self.0.lock().unwrap() = FakeAnswer::Addrs(addrs);
        }

        fn fail(&self, message: &str) {
            *self.0.lock().unwrap() = FakeAnswer::Fail(message.to_string());
        }
    }

    #[async_trait]
    impl EndpointResolver for FakeResolver {
        async fn resolve(&self, _hosts: &[String]) -> Result<Vec<IpAddr>, DaemonError> {
            match &*self.0.lock().unwrap() {
                FakeAnswer::Addrs(addrs) => Ok(addrs.clone()),
                FakeAnswer::Fail(message) => Err(DaemonError::HostNet(message.clone())),
            }
        }
    }

    /// Фейк-шлюзы: v4 всегда есть, v6 — опционально (нет — «шлюз не
    /// найден», кейс §9).
    struct FakeGateways {
        v6: Option<(IpAddr, String)>,
    }

    #[async_trait]
    impl GatewaySource for FakeGateways {
        async fn gateway(
            &self,
            _exclude_idx: &[u32],
            family: GwFamily,
        ) -> Result<(IpAddr, String), DaemonError> {
            match family {
                GwFamily::V4 => Ok((ip("192.168.1.1"), "Wi-Fi".to_string())),
                GwFamily::V6 => self
                    .v6
                    .clone()
                    .ok_or_else(|| DaemonError::HostNet("физический шлюз не найден".to_string())),
            }
        }
    }

    /// Фейк-адаптеры: текущий список; «появление» VPN-адаптера делает
    /// фейк-спавнер при запуске клиента.
    struct FakeAdapters(StdMutex<Vec<NetAdapter>>);

    #[async_trait]
    impl AdapterSource for FakeAdapters {
        async fn list_adapters(&self) -> io::Result<Vec<NetAdapter>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// Фейк-спавнер клиента: журнал + «создание» адаптеров пакетами
    /// на каждый запуск (первый спавн — первый пакет, и т.д.; исчерпание
    /// — новый клиент интерфейса не создаёт, кейс таймаута).
    struct FakeSpawner {
        ops: Ops,
        adapters: Option<Arc<FakeAdapters>>,
        appear: StdMutex<VecDeque<Vec<NetAdapter>>>,
    }

    #[async_trait]
    impl ClientSpawner for FakeSpawner {
        async fn spawn(&self, _client: &str, _args: &[String]) -> io::Result<()> {
            self.ops.push("spawn");
            if let Some(adapters) = &self.adapters {
                if let Some(appear) = self.appear.lock().unwrap().pop_front() {
                    adapters.0.lock().unwrap().extend(appear);
                }
            }
            Ok(())
        }
    }

    /// Фейк-биндер: журнал вызовов привязки.
    struct FakeBinder {
        ops: Ops,
    }

    impl IfaceBinder for FakeBinder {
        fn bind(&self, _sock: &socket2::Socket, ifindex: u32, _v6: bool) -> io::Result<()> {
            self.ops.push(format!("bind(iface={ifindex})"));
            Ok(())
        }
    }

    // ------------------------------------------------------------------
    // Оснастка.
    // ------------------------------------------------------------------

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn adapter(alias: &str, idx: u32, up: bool) -> NetAdapter {
        NetAdapter {
            alias: alias.to_string(),
            idx,
            up,
        }
    }

    /// Host-профиль: endpoint + один forward-проброс.
    fn host_profile(id: &str, endpoint: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Хост {id}"),
            kind: BottleKind::Host,
            vpn: VpnConfig::Host {
                client: r"C:\Program Files\Vendor\vpn.exe".to_string(),
                args: Some("connect office".to_string()),
                endpoints: vec![endpoint.to_string()],
                interface: None,
            },
            expose: ExposeConfig {
                socks5: 0,
                forwards: vec![ForwardSpec {
                    listen: 0,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: None,
                launch: None,
            },
        }
    }

    /// Конфигурация стенда.
    struct TestSetup {
        resolver: Arc<FakeResolver>,
        svc_factory: Arc<FakeSvcFactory>,
        /// Снапшоты адаптеров (тест убивает «интерфейсы» через retain).
        adapters: Arc<FakeAdapters>,
        host: HostManager,
        ops: Ops,
    }

    /// Собирает менеджера на фейках: v4-шлюз всегда есть, v6 — по
    /// запросу теста; VPN-адаптер(ы) «появляются» при первом спавне
    /// клиента.
    fn test_host(v6_gateway: bool, appear: Vec<NetAdapter>) -> TestSetup {
        test_host_opts(v6_gateway, vec![appear], false, None)
    }

    /// Стенд с расписанием «появлений» адаптеров по спавнам: i-й запуск
    /// клиента создаёт i-й пакет адаптеров.
    fn test_host_spawns(v6_gateway: bool, spawn_appear: Vec<Vec<NetAdapter>>) -> TestSetup {
        test_host_opts(v6_gateway, spawn_appear, false, None)
    }

    /// Стенд с опциями: отказ первого svc.Add по транспорту, отключённая
    /// служба маршрутов.
    fn test_host_opts(
        v6_gateway: bool,
        spawn_appear: Vec<Vec<NetAdapter>>,
        fail_first_add: bool,
        svc_present: Option<fn() -> bool>,
    ) -> TestSetup {
        let ops = Ops::default();
        let resolver = Arc::new(FakeResolver(StdMutex::new(FakeAnswer::Addrs(vec![ip(
            "203.0.113.10",
        )]))));
        let adapters = Arc::new(FakeAdapters(StdMutex::new(vec![
            adapter("Wi-Fi", 12, true),
            adapter("Ethernet 2", 11, false),
        ])));
        let spawner = FakeSpawner {
            ops: ops.clone(),
            adapters: Some(Arc::clone(&adapters)),
            appear: StdMutex::new(spawn_appear.into()),
        };
        let svc_factory = Arc::new(FakeSvcFactory {
            ops: ops.clone(),
            fail_adds_left: Arc::new(StdMutex::new(u32::from(fail_first_add))),
            gate_next: StdMutex::new(None),
        });
        let parts = HostParts {
            svc_present: svc_present.unwrap_or(|| true),
            svc_factory: Arc::clone(&svc_factory) as Arc<dyn SvcFactory>,
            adapters: Arc::clone(&adapters) as Arc<dyn AdapterSource>,
            spawner: Arc::new(spawner),
            gateways: Arc::new(FakeGateways {
                v6: v6_gateway.then(|| (ip("2001:db8::1"), "Wi-Fi".to_string())),
            }),
            resolver: Arc::clone(&resolver) as Arc<dyn EndpointResolver>,
            binder: Arc::new(FakeBinder { ops: ops.clone() }),
        };
        let timing = HostTiming {
            iface_timeout: Duration::from_millis(200),
            iface_step: Duration::from_millis(10),
        };
        TestSetup {
            resolver,
            svc_factory,
            adapters,
            host: HostManager::new(parts, timing),
            ops,
        }
    }

    /// Эхо-сервер, пишущий «accepted» в журнал при соединении.
    async fn echo_with_log(addr_ops: Ops) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                addr_ops.push("accepted");
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// Минимальный SOCKS5-клиент: greeting → method; возвращает сокет.
    async fn socks_negotiate(socks5: u16) -> TcpStream {
        let mut sock = TcpStream::connect(("127.0.0.1", socks5))
            .await
            .expect("подключение к socks5 демона");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00", "no-auth выбран");
        sock
    }

    // ------------------------------------------------------------------
    // Сценарии рулинга T9 (1–6, 8) + poison.
    // ------------------------------------------------------------------

    /// (1) Полный успех: svc.add endpoint'ов ДО спавна; SOCKS5
    /// обслуживает CONNECT через привязанный к адаптеру диал (порядок
    /// bind → connect пинится журналом); статус up с портами.
    #[tokio::test]
    // На Windows-раннере виснет цепочка SOCKS5-relay (заглушено; покрытие —
    // unix CI + приёмка M2 на реальной Windows).
    #[cfg_attr(target_os = "windows", ignore = "hangs on windows CI runner")]
    async fn full_success_routes_before_spawn_relay_bound_and_up() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup.host.start(&profile).await.expect("start ок");
        assert_eq!(reply, CliReply::Started { id: "corp".into() });

        // Порядок: маршрут endpoint'а установлен ДО запуска клиента.
        let ops = setup.ops.snapshot();
        assert!(
            setup.ops.first("svc:Add(203.0.113.10)") < setup.ops.first("spawn"),
            "svc.add до спавна: {ops:?}"
        );
        assert_eq!(setup.ops.count("spawn"), 1, "клиент запущен один раз");

        // Статус: up, порты назначены.
        let bottles = setup.host.status().await;
        assert_eq!(bottles.len(), 1);
        assert!(matches!(bottles[0].state, BottleState::Up));
        let socks5 = bottles[0].socks5.expect("порт socks5 назначен");
        assert_eq!(bottles[0].forwards.len(), 1);

        // SOCKS5 живой: negotiate + CONNECT в эхо за «туннелем»; диал
        // привязан к адаптеру 42 ДО установления соединения (журнал).
        let echo = echo_with_log(setup.ops.clone()).await;
        let mut sock = socks_negotiate(socks5).await;
        let [a, b, c, d] = match echo.ip() {
            IpAddr::V4(v4) => v4.octets(),
            IpAddr::V6(_) => panic!("эхо на v4"),
        };
        let port = echo.port();
        sock.write_all(&[
            0x05,
            0x01,
            0x00,
            0x01,
            a,
            b,
            c,
            d,
            (port >> 8) as u8,
            port as u8,
        ])
        .await
        .expect("CONNECT");
        let mut reply = [0u8; 10];
        sock.read_exact(&mut reply).await.expect("ответ CONNECT");
        assert_eq!(reply[1], 0x00, "CONNECT успешен");

        // «accepted» пишется эхо-сервером асинхронно ПОСЛЕ kernel-
        // handshake (userspace accept): ждём появления записи в
        // журнале до order-ассерта, иначе он гонится с логом (~9%
        // флейков). Продуктовая логика детерминирована: bind
        // выполняется до connect.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while setup.ops.count("accepted") == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "эхо не отметило соединение за 5 с: {:?}",
                setup.ops.snapshot()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Review Focus #5: привязка сокета к адаптеру — до соединения.
        assert!(
            setup.ops.first("bind(iface=42)") < setup.ops.first("accepted"),
            "bind до connect: {:?}",
            setup.ops.snapshot()
        );

        // Эхо через «туннель».
        sock.write_all(b"host ping").await.expect("данные");
        let mut echoed = [0u8; 9];
        tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut echoed))
            .await
            .expect("эхо в пределах тайм-аута")
            .expect("чтение эха");
        assert_eq!(&echoed, b"host ping");
    }

    /// (1b) Маршруты чужих endpoint'ов переустанавливаются ДО запуска
    /// второго клиента (спека §7.2 шаг 2).
    #[tokio::test]
    async fn second_start_readds_foreign_endpoints_before_spawn() {
        let setup = test_host_spawns(
            true,
            vec![
                vec![adapter("CorpVPN", 42, true)],
                vec![adapter("PartnerVPN", 44, true)],
            ],
        );
        let first = host_profile("corp", "vpn.example.com");
        setup.host.start(&first).await.expect("первая бутылка");

        // Вторая бутылка со своим endpoint'ом: резолвер теперь A2.
        let second = host_profile("partner", "partner.example.com");
        setup.resolver.set(vec![ip("198.51.100.20")]);
        let reply = setup.host.start(&second).await.expect("start ок");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "partner".into()
            }
        );

        // Перед вторым спавном стоят маршруты ОБЕИХ бутылок: своей и
        // чужой (первой).
        let ops = setup.ops.snapshot();
        let spawns: Vec<usize> = ops
            .iter()
            .enumerate()
            .filter(|(_, e)| *e == "spawn")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(spawns.len(), 2, "два клиента запущены: {ops:?}");
        assert!(
            setup.ops.first("svc:Add(203.0.113.10)") < spawns[1],
            "маршрут чужой бутылки до второго спавна: {ops:?}"
        );
        assert!(
            setup.ops.first("svc:Add(198.51.100.20)") < spawns[1],
            "маршрут своей бутылки до второго спавна: {ops:?}"
        );

        // Обе бутылки вверх.
        assert_eq!(setup.host.status().await.len(), 2);
    }

    /// (2) Служба маршрутов не установлена — текст T7.
    #[tokio::test]
    async fn missing_svc_answers_installation_message() {
        let setup = test_host_opts(true, vec![], false, Some(|| false));
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup.host.start(&profile).await.expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(message, SVC_MISSING_MESSAGE);
        assert!(setup.ops.snapshot().is_empty(), "ничего не делается");
        assert!(setup.host.status().await.is_empty());
    }

    /// (3) Интерфейс не появился за сжатый таймаут — failed с текстом
    /// таймаута, маршруты откачены (8).
    #[tokio::test]
    async fn iface_timeout_fails_and_rolls_routes_back() {
        let setup = test_host(true, vec![]); // спавнер не создаёт адаптер
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup.host.start(&profile).await.expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("интерфейс не появился за"),
            "текст таймаута: {message}"
        );

        // Откат: маршрут endpoint'а снят (своей сессии).
        let ops = setup.ops.snapshot();
        assert!(
            setup.ops.first("svc:Add(203.0.113.10)") < setup.ops.first("svc:Del(203.0.113.10)"),
            "после неудачи маршрут снят: {ops:?}"
        );

        // Бутылка failed с причиной; портов нет.
        let bottles = setup.host.status().await;
        assert_eq!(bottles.len(), 1);
        assert!(
            matches!(&bottles[0].state, BottleState::Failed { reason } if reason.contains("не появился")),
            "{:?}",
            bottles[0].state
        );
        assert_eq!(bottles[0].socks5, None);
    }

    /// (4) Несколько новых интерфейсов — failed «укажите interface».
    #[tokio::test]
    async fn two_new_adapters_fail_asking_for_interface() {
        let setup = test_host(
            true,
            vec![adapter("CorpVPN", 42, true), adapter("OtherVPN", 43, true)],
        );
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup.host.start(&profile).await.expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("появилось несколько интерфейсов")
                && message.contains("укажите interface в профиле")
                && message.contains("CorpVPN")
                && message.contains("OtherVPN"),
            "перечислены новые интерфейсы: {message}"
        );
        // Маршруты откачены.
        assert!(setup
            .ops
            .snapshot()
            .iter()
            .any(|e| e == "svc:Del(203.0.113.10)"));
    }

    /// (5) Явный `interface` из профиля: берётся именованный адаптер,
    /// даже когда новых несколько.
    #[tokio::test]
    // На Windows-раннере виснет цепочка SOCKS5-relay (заглушено; покрытие —
    // unix CI + приёмка M2 на реальной Windows).
    #[cfg_attr(target_os = "windows", ignore = "hangs on windows CI runner")]
    async fn explicit_interface_wins_over_multiple_new() {
        let setup = test_host(
            true,
            vec![adapter("CorpVPN", 42, true), adapter("OtherVPN", 43, true)],
        );
        let mut profile = host_profile("corp", "vpn.example.com");
        if let VpnConfig::Host { interface, .. } = &mut profile.vpn {
            *interface = Some("corpvpn".to_string()); // кейс-инсенситивно
        }

        let reply = setup.host.start(&profile).await.expect("start ок");
        assert_eq!(reply, CliReply::Started { id: "corp".into() });

        // Релеи привязаны к именованному адаптеру (индекс 42, не 43):
        // CONNECT запускает диал (привязка логируется до ответа — даже
        // на отказный порт).
        let bottles = setup.host.status().await;
        assert!(matches!(bottles[0].state, BottleState::Up));
        let socks5 = bottles[0].socks5.expect("порт назначен");
        let mut sock = socks_negotiate(socks5).await;
        // 127.0.0.1:9 — порт без слушателя: диал выполнится и откажется,
        // но привязка к адаптеру произойдёт ДО попытки соединения.
        sock.write_all(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x09])
            .await
            .expect("CONNECT");
        let mut reply = [0u8; 10];
        sock.read_exact(&mut reply)
            .await
            .expect("ответ CONNECT (диал выполнен)");
        assert!(
            setup.ops.snapshot().iter().any(|e| e == "bind(iface=42)"),
            "привязка к именованному адаптеру: {:?}",
            setup.ops.snapshot()
        );
    }

    /// (6a) v6-only endpoint без v6-шлюза — блокировка с текстом §9.
    #[tokio::test]
    async fn v6_only_endpoint_without_v6_gateway_blocks_start() {
        let setup = test_host(false, vec![adapter("CorpVPN", 42, true)]);
        setup.resolver.set(vec![ip("2001:db8::53")]);
        let profile = host_profile("corp", "vpn6.example.com");

        let reply = setup.host.start(&profile).await.expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("vpn6.example.com")
                && message.contains("IPv6")
                && message.contains("нет IPv6-шлюза")
                && message.contains("запуск заблокирован")
                && message.contains("§9"),
            "текст блокировки §9: {message}"
        );
        // Клиент не запускался.
        assert_eq!(setup.ops.count("spawn"), 0);
        let bottles = setup.host.status().await;
        assert!(
            matches!(&bottles[0].state, BottleState::Failed { reason } if reason.contains("§9")),
            "{:?}",
            bottles[0].state
        );
    }

    /// (6b) v4+v6 хост при недоступном v6-шлюзе — старт по v4; v6-адрес
    /// маршрута не получает.
    #[tokio::test]
    async fn v4_and_v6_host_starts_on_v4_only() {
        let setup = test_host(false, vec![adapter("CorpVPN", 42, true)]);
        setup
            .resolver
            .set(vec![ip("203.0.113.10"), ip("2001:db8::53")]);
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup.host.start(&profile).await.expect("start ок");
        assert_eq!(reply, CliReply::Started { id: "corp".into() });
        let ops = setup.ops.snapshot();
        assert!(
            ops.iter().any(|e| e == "svc:Add(203.0.113.10)"),
            "v4-endpoint маршрутизирован: {ops:?}"
        );
        assert!(
            !ops.iter().any(|e| e.contains("2001:db8::53")),
            "v6-адрес без v6-шлюза не маршрутизируется: {ops:?}"
        );
    }

    /// Poison-политика: io-ошибка транспорта пересоздаёт клиент службы
    /// (новый `Hello`) и повторяет операцию.
    #[tokio::test]
    async fn transport_failure_recreates_svc_client_and_retries() {
        let setup = test_host_opts(true, vec![vec![adapter("CorpVPN", 42, true)]], true, None);
        let profile = host_profile("corp", "vpn.example.com");

        let reply = setup
            .host
            .start(&profile)
            .await
            .expect("start после ретрая");
        assert_eq!(reply, CliReply::Started { id: "corp".into() });

        let ops = setup.ops.snapshot();
        assert_eq!(
            setup.ops.count("svc:Hello"),
            2,
            "клиент пересоздан после io-ошибки: {ops:?}"
        );
        assert!(
            ops.iter().any(|e| e == "svc:Add(203.0.113.10)"),
            "повтор add успешен: {ops:?}"
        );
    }

    /// Poison-карусель и чужие маршруты: пересоздание клиента убивает
    /// сессию службы (маршруты снимаются, спека §9) — живое множество
    /// АКТИВНОЙ бутылки переустанавливается новым клиентом до повтора
    /// операции (иначе активная бутылка молча теряла бы pin-маршрут).
    #[tokio::test]
    async fn poison_recreate_restores_live_routes_of_active_bottle() {
        let setup = test_host_spawns(
            true,
            vec![
                vec![adapter("CorpVPN", 42, true)],
                vec![adapter("PartnerVPN", 44, true)],
            ],
        );
        let first = host_profile("corp", "vpn.example.com");
        setup.host.start(&first).await.expect("первая бутылка жива");

        // Вторая бутылка: её первый svc.Add сажает транспорт (poison).
        setup.svc_factory.fail_next_add();
        setup.resolver.set(vec![ip("198.51.100.20")]);
        let second = host_profile("partner", "partner.example.com");
        let reply = setup.host.start(&second).await.expect("start после ретрая");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "partner".into()
            }
        );

        let ops = setup.ops.snapshot();
        assert_eq!(
            setup.ops.count("svc:Hello"),
            2,
            "клиент пересоздан: {ops:?}"
        );
        // Маршрут активной первой бутылки переустановлен ПОСЛЕ
        // пересоздания клиента (второй Hello) — сессия-наследник получила
        // живое множество заново.
        let last_hello = ops
            .iter()
            .rposition(|e| e == "svc:Hello")
            .expect("Hello в журнале");
        let last_restore_add = ops
            .iter()
            .rposition(|e| e == "svc:Add(203.0.113.10)")
            .expect("переустановка маршрута первой бутылки");
        assert!(
            last_restore_add > last_hello,
            "чужой pin-маршрут восстановлен после пересоздания: {ops:?}"
        );
        // Обе бутылки подняты.
        assert_eq!(setup.host.status().await.len(), 2);
    }

    /// Stop: снимает маршруты сессии, гасит релеи и убирает бутылку из
    /// статуса; ответ несёт предупреждение об оставленном VPN-клиенте
    /// (спека §7.2 шаг 6); повторный stop — понятная ошибка.
    #[tokio::test]
    async fn stop_removes_routes_relays_and_status() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let profile = host_profile("corp", "vpn.example.com");
        setup.host.start(&profile).await.expect("start");
        let socks5 = setup.host.status().await[0].socks5.expect("порт назначен");

        let reply = setup.host.stop("corp").await.expect("stop ок");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "corp".into(),
                warning: Some(HOST_STOP_CLIENT_WARNING.to_string()),
            }
        );

        assert!(
            setup
                .ops
                .snapshot()
                .iter()
                .any(|e| e == "svc:Del(203.0.113.10)"),
            "маршрут сессии снят"
        );
        assert!(setup.host.status().await.is_empty());
        assert!(
            TcpStream::connect(("127.0.0.1", socks5)).await.is_err(),
            "порт закрыт после stop"
        );

        // Повторный stop — no-op с предупреждением (спека §5.2).
        let reply = setup.host.stop("corp").await.expect("ответ, не сбой");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "corp".into(),
                warning: Some("не была запущена".to_string())
            }
        );
    }

    /// Сериализация восстановления (finding 2 ревью): ДВА ОДНОВРЕМЕННЫХ
    /// poison-события (параллельные проходы DNS-цикла) переустанавливают
    /// живое множество АКТИВНОЙ сессии ровно один раз — конкурент
    /// застаёт гвард восстановления занятым, дожидается чужого
    /// восстановления и пропускает своё.
    ///
    /// Хореография (детерминирована на current_thread-runtime: `join!`
    /// опрашивает ветки по порядку, парковка в гейте — единственная
    /// точка передачи управления): проход-1 своим Add(B) сажает
    /// транспорт, берёт гвард восстановления и паркуется в его Add(A) —
    /// гейт держит первый Add транспорта до следующего Add на нём.
    /// Проход-2 вычисляет fresh=[B] (проход-1 ещё не зафиксировал B),
    /// своим Add(B) — на ТОМ ЖЕ восстановительном транспорте — получает
    /// второй io-отказ, будит гейт и застаёт гвард занятым → ветка
    /// ожидания (`restore_waits`). Дальше держатель завершает
    /// восстановление, каждый проход доводит только СВОЙ Add(B).
    #[tokio::test]
    async fn simultaneous_poisons_restore_live_routes_once() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let profile = host_profile("corp", "vpn.example.com");
        setup.host.start(&profile).await.expect("start");
        // t=0: A маршрутизирован (один успешный Add(A)).
        assert_eq!(exact_count(&setup, "svc:Add(203.0.113.10)"), 1);

        // Оба прохода увидят новый адрес B (фиксации ещё нет — проход-1
        // стоит в гейте восстановления). Оба СВОИХ Add(B) сажают
        // транспорт; Add(A) восстановления проходит успешно уже после
        // гейта (счётчик к тому моменту исчерпан).
        setup
            .resolver
            .set(vec![ip("203.0.113.10"), ip("198.51.100.77")]);
        setup.svc_factory.fail_next_adds(2);
        setup.svc_factory.gate_next_add();
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(async { setup.host.recheck_dns().await }, async {
                setup.host.recheck_dns().await
            },)
        })
        .await
        .expect("хореография двойного poison не сошлась за 5 с");

        // Оба io-отказа — по одному Add(B) на попытку каждого прохода.
        let ops = setup.ops.snapshot();
        assert_eq!(
            setup.ops.count("svc:Add(198.51.100.77)~io"),
            2,
            "по одному io-отказу Add(B) на проход: {ops:?}"
        );
        // Восстановление A ровно ОДНО: исходный Add(A) + Add(A) держателя
        // гварда; дублей нет — конкурент своё восстановление пропустил.
        assert_eq!(
            exact_count(&setup, "svc:Add(203.0.113.10)"),
            2,
            "восстановление A ровно одно: {ops:?}"
        );
        // Ретраи своих операций: Add(B) успешен дважды (по разу на
        // проход), на свежем транспорте после второго poison.
        assert_eq!(
            exact_count(&setup, "svc:Add(198.51.100.77)"),
            2,
            "по одному успешному Add(B) на проход: {ops:?}"
        );
        // Транспорт пересоздан дважды: восстановление держателя и ретрай
        // прохода-1 (кэш ссажен poison'ом прохода-2); плюс исходный.
        assert_eq!(
            setup.ops.count("svc:Hello"),
            3,
            "исходный транспорт + два пересоздания: {ops:?}"
        );
        // ПИН ветки ожидания: ровно один конкурент (проход-2) дождался
        // чужого восстановления и пропустил своё.
        assert_eq!(
            setup
                .host
                .restore_waits
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ветка «дождаться и пропустить» исполнена ровно раз"
        );

        // Бутылка жива, B зафиксирован в живом множестве (следующий
        // проход ничего не добавляет).
        setup.host.recheck_dns().await;
        assert_eq!(
            exact_count(&setup, "svc:Add(198.51.100.77)"),
            2,
            "адрес зафиксирован после успешной установки"
        );
        assert!(matches!(
            setup.host.status().await[0].state,
            BottleState::Up
        ));
    }

    /// Число записей журнала, ТОЧНО равных `entry` (без ~io-хвостов).
    fn exact_count(setup: &TestSetup, entry: &str) -> usize {
        setup
            .ops
            .snapshot()
            .iter()
            .filter(|e| e.as_str() == entry)
            .count()
    }

    /// Маршрутизация менеджера: kind=Host уходит в host-менеджер,
    /// статус объединён, stop гасит.
    #[tokio::test]
    async fn manager_routes_host_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store
            .save(&host_profile("corp", "vpn.example.com"))
            .unwrap();

        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let TestSetup { host, .. } = setup;
        let manager = BottleManager::new_with_host(store, Arc::new(FailingAgentConnect), host);

        let reply = manager
            .start("corp", None)
            .await
            .expect("start через менеджера");
        assert_eq!(reply, CliReply::Started { id: "corp".into() });

        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 1);
        assert!(matches!(bottles[0].state, BottleState::Up));
        assert!(bottles[0].socks5.is_some());

        let reply = manager.stop("corp").await.expect("stop через менеджера");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "corp".into(),
                warning: Some(HOST_STOP_CLIENT_WARNING.to_string()),
            }
        );
        assert!(manager.status().await.is_empty());
    }

    // ------------------------------------------------------------------
    // DNS-цикл T10 (спека §7.2, шаг 5).
    // ------------------------------------------------------------------

    /// (7) DNS-таймлайн: t=0 — только A (проход без изменений ничего не
    /// делает); t=60 — A+B: новый адрес B получает маршрут, прежний A
    /// жив (никаких Del); stop снимает оба адреса накопленного множества.
    #[tokio::test]
    async fn dns_recheck_adds_new_addr_old_alive_stop_removes_both() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let profile = host_profile("corp", "vpn.example.com");
        setup.host.start(&profile).await.expect("start");
        assert_eq!(
            setup.ops.count("svc:Add"),
            1,
            "t=0: маршрут только A (203.0.113.10)"
        );

        // Проход без изменений: повторного add и снятий нет.
        setup.host.recheck_dns().await;
        assert_eq!(setup.ops.count("svc:Add"), 1, "A уже в живом множестве");
        assert_eq!(setup.ops.count("svc:Del"), 0, "живой адрес не снимается");

        // t=60: резолвер теперь A+B.
        setup
            .resolver
            .set(vec![ip("203.0.113.10"), ip("198.51.100.77")]);
        setup.host.recheck_dns().await;
        assert_eq!(
            setup.ops.count("svc:Add"),
            2,
            "новый адрес B маршрутизирован"
        );
        let ops = setup.ops.snapshot();
        assert!(
            ops.iter().any(|e| e == "svc:Add(198.51.100.77)"),
            "add именно нового адреса: {ops:?}"
        );
        assert!(
            !ops.iter().any(|e| e.starts_with("svc:Del")),
            "прежний адрес A жив до stop (спека §7.2): {ops:?}"
        );

        // Сессия по-прежнему up; stop снимает оба адреса сессии.
        assert!(matches!(
            setup.host.status().await[0].state,
            BottleState::Up
        ));
        setup.host.stop("corp").await.expect("stop");
        let ops = setup.ops.snapshot();
        assert!(ops.iter().any(|e| e == "svc:Del(203.0.113.10)"), "{ops:?}");
        assert!(ops.iter().any(|e| e == "svc:Del(198.51.100.77)"), "{ops:?}");
    }

    /// Ошибка резолва в DNS-цикле — warn, не fatal: сессия жива, живое
    /// множество не меняется; восстановившийся резолвер работает дальше.
    #[tokio::test]
    async fn dns_recheck_resolve_failure_is_warn_not_fatal() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let profile = host_profile("corp", "vpn.example.com");
        setup.host.start(&profile).await.expect("start");

        setup.resolver.fail("резолвер недоступен (фейк)");
        setup.host.recheck_dns().await;
        assert!(
            matches!(setup.host.status().await[0].state, BottleState::Up),
            "ошибка резолва не роняет сессию"
        );
        assert_eq!(setup.ops.count("svc:Add"), 1, "новых маршрутов нет");
        assert_eq!(setup.ops.count("svc:Del"), 0, "существующие не сняты");

        // Резолвер восстановился — новый адрес получает маршрут.
        setup
            .resolver
            .set(vec![ip("203.0.113.10"), ip("198.51.100.77")]);
        setup.host.recheck_dns().await;
        assert_eq!(setup.ops.count("svc:Add"), 2, "после восстановления — add");
    }

    /// Пин периода цикла — 30 с (спека §7.2, шаг 5).
    #[test]
    fn dns_recheck_step_pins_spec_value() {
        assert_eq!(DNS_RECHECK_STEP, Duration::from_secs(30));
    }

    // ------------------------------------------------------------------
    // Чистые функции.
    // ------------------------------------------------------------------

    /// Пин значения IP_UNICAST_IF: v4 — индекс в сетевом порядке байтов
    /// (байты значения, уходящие в setsockopt, — big-endian), v6 —
    /// хостовый порядок (MSDN).
    #[test]
    fn unicast_if_value_byte_order() {
        assert_eq!(unicast_if_value(42, false), 42u32.to_be());
        // Уровень байтов: 4 байта значения на проводе setsockopt —
        // индекс 1 в сетевом (big-endian) порядке.
        assert_eq!(unicast_if_value(1, false).to_ne_bytes(), [0, 0, 0, 1]);
        assert_eq!(unicast_if_value(42, true), 42);
    }

    /// Разбор вывода Get-NetAdapter: имена с пробелами, статусы,
    /// мусорные строки пропускаются.
    #[test]
    fn parse_adapters_handles_spaced_aliases_and_statuses() {
        let text = "Corp VPN  42  Up\nWi-Fi  12  Disconnected\n\
                    Ethernet 2  11  Disabled\n\
                    мусор\n\
                    not-an-idx  Up  x\n";
        let adapters = parse_adapters(text);
        assert_eq!(
            adapters,
            vec![
                adapter("Corp VPN", 42, true),
                adapter("Wi-Fi", 12, false),
                adapter("Ethernet 2", 11, false),
            ]
        );
    }

    /// Форма PowerShell-запроса адаптеров: читающий пайплайн с шириной
    /// вывода и без заголовков.
    #[test]
    fn adapters_ps_pins_readonly_query_shape() {
        let args = adapters_ps();
        assert_eq!(args[0], "Get-NetAdapter");
        assert!(
            args.iter().any(|a| a == "-HideTableHeaders"),
            "колонки без заголовков: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["-Width", "512"]),
            "ширина вывода: {args:?}"
        );
        assert!(!args.iter().any(|a| a.contains("New-NetAdapter")));
        assert!(!args.iter().any(|a| a.contains("Disable-NetAdapter")));
    }

    /// Фейк-подключение к агенту: host-путь до него не доходит.
    struct FailingAgentConnect;

    #[async_trait]
    impl crate::manager::AgentConnect for FailingAgentConnect {
        async fn connect(&self) -> Result<crate::appliance::ApplianceHandle, DaemonError> {
            Err(DaemonError::Appliance(
                "агент недоступен (фейк host-теста)".to_string(),
            ))
        }

        async fn alive(&self) -> bool {
            false
        }
    }

    // ------------------------------------------------------------------
    // Живучесть T11 (спека §9 + рулинг префлайта): живость интерфейса,
    // смерти relay-задач, stop при удалённом профиле.
    // ------------------------------------------------------------------

    /// (а-host) Health-проход: host-бутылка с ЖИВЫМ интерфейсом не
    /// тронута (рулинг — проверка живости, не слепой stop), с умершим —
    /// полный host-stop путь: метка `stopped`, маршруты её endpoint'ов
    /// сняты, чужие живут. Проверка идёт через менеджера (wiring
    /// health_pass) и при МЁРТВОМ агенте — host-бутылки от него не
    /// зависят.
    #[tokio::test]
    async fn health_pass_stops_host_bottle_on_dead_iface_and_spares_alive() {
        let setup = test_host_spawns(
            true,
            vec![
                vec![adapter("CorpVPN", 42, true)],
                vec![adapter("PartnerVPN", 44, true)],
            ],
        );
        let TestSetup {
            resolver,
            host,
            adapters,
            ops,
            ..
        } = setup;

        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store
            .save(&host_profile("corp", "vpn.example.com"))
            .unwrap();
        store
            .save(&host_profile("partner", "partner.example.com"))
            .unwrap();
        let manager = BottleManager::new_with_host(
            Store::open(store.data_root()).unwrap(),
            Arc::new(FailingAgentConnect),
            host,
        );

        manager.start("corp", None).await.expect("первая бутылка");
        // Вторая бутылка резолвится в другой адрес (B).
        resolver.set(vec![ip("198.51.100.20")]);
        manager
            .start("partner", None)
            .await
            .expect("вторая бутылка");
        let corp_socks5 = manager
            .status()
            .await
            .into_iter()
            .find(|b| b.id == "corp")
            .unwrap()
            .socks5
            .expect("порт corp");

        // Интерфейс второй бутылки исчез (адаптер 44 removed).
        adapters.0.lock().unwrap().retain(|a| a.idx != 44);

        // Health-проход (агент мёртв — host-путь всё равно исполняется).
        manager.health_pass().await;

        let bottles = manager.status().await;
        let corp_state = bottles
            .iter()
            .find(|b| b.id == "corp")
            .unwrap_or_else(|| panic!("corp жива: {bottles:?}"));
        assert!(matches!(corp_state.state, BottleState::Up), "{bottles:?}");
        let partner_state = bottles
            .iter()
            .find(|b| b.id == "partner")
            .unwrap_or_else(|| panic!("partner видна с меткой: {bottles:?}"));
        assert!(
            matches!(partner_state.state, BottleState::Stopped),
            "умерший интерфейс → stopped: {:?}",
            partner_state.state
        );
        assert_eq!(partner_state.socks5, None, "релеи погашены");

        // Маршрут умершей сессии снят, живой — на месте.
        let ops_now = ops.snapshot();
        assert!(
            ops_now.iter().any(|e| e == "svc:Del(198.51.100.20)"),
            "svc.del адреса умершей сессии: {ops_now:?}"
        );
        assert!(
            !ops_now.iter().any(|e| e == "svc:Del(203.0.113.10)"),
            "маршрут живой сессии не снят: {ops_now:?}"
        );

        // Живая бутылка работает: SOCKS5 negotiate проходит.
        let mut sock = socks_negotiate(corp_socks5).await;
        sock.shutdown().await.ok();

        // Повторный проход ничего не меняет (stopped не активна).
        manager.health_pass().await;
        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 2, "метки стабильны: {bottles:?}");
        assert!(matches!(
            bottles.iter().find(|b| b.id == "corp").unwrap().state,
            BottleState::Up
        ));
    }

    /// (г) carried minor T9/T10: stop при удалённом/битом профиле всё
    /// равно разбирает host-сессию (маршруты сняты) — routing по
    /// runtime-состоянию, не по kind из профиля.
    #[tokio::test]
    async fn stop_with_deleted_profile_still_tears_down_host_session() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let TestSetup { host, ops, .. } = setup;

        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store
            .save(&host_profile("corp", "vpn.example.com"))
            .unwrap();
        let manager = BottleManager::new_with_host(
            Store::open(store.data_root()).unwrap(),
            Arc::new(FailingAgentConnect),
            host,
        );
        manager.start("corp", None).await.expect("start");

        // Профиль удалён (load → NotFound): раньше stop уходил в env-ветку
        // «не запущена»; теперь сессия разбирается по runtime-состоянию.
        std::fs::remove_file(store.data_root().join("bottles/corp.toml")).unwrap();

        let reply = manager.stop("corp").await.expect("stop без сбоя");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "corp".into(),
                warning: Some(HOST_STOP_CLIENT_WARNING.to_string()),
            }
        );
        assert!(
            ops.snapshot().iter().any(|e| e == "svc:Del(203.0.113.10)"),
            "маршруты сессии сняты несмотря на удалённый профиль"
        );
        assert!(manager.status().await.is_empty());
    }

    /// (в-host) Смерть relay-задачи host-сессии → `Degraded{причина}`:
    /// сессия активна (stop работает, маршруты живут). Доклад о смерти —
    /// тот же вызов, что делает задача релея (см. report_relay_death_for_test).
    #[tokio::test]
    async fn host_relay_death_degrades_session() {
        let setup = test_host(true, vec![adapter("CorpVPN", 42, true)]);
        let TestSetup { host, ops, .. } = setup;
        let profile = host_profile("corp", "vpn.example.com");
        host.start(&profile).await.expect("start");

        host.report_relay_death_for_test(
            "corp",
            "релей 127.0.0.1:49222 остановлен: сбой слушателя",
        );
        host.collect_relay_deaths().await;

        let bottles = host.status().await;
        assert!(
            matches!(
                &bottles[0].state,
                BottleState::Degraded { reason } if reason.contains("релей")
            ),
            "деградация с причиной релея: {:?}",
            bottles[0].state
        );
        // Сессия жива: маршруты не сняты, stop отрабатывает.
        assert!(
            !ops.snapshot().iter().any(|e| e.starts_with("svc:Del")),
            "маршруты деградированной сессии живы"
        );
        let reply = host.stop("corp").await.expect("stop деградированной");
        assert!(matches!(reply, CliReply::Stopped { .. }));
    }
}
