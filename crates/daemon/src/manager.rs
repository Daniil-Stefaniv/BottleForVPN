//! Жизненный цикл env-бутылки: оркестрация по спеке §7.1.
//!
//! [`BottleManager::start`] резервирует порты на `127.0.0.1`, поднимает
//! бутылку в аплаенсе (`Up` → `Ready`) и перестраивает зарезервированные
//! слушатели на [`serve_forward`] `127.0.0.1:port → vm_ip:port`; `stop`
//! отправляет агенту `Down` и гасит задачи и слушатели. Демон всегда
//! резолвит порты сам (диапазон 49152–65535 при автоподборе) — агенту
//! уходят только конкретные номера, `socks5_port = 0` не отправляется.
//!
//! Протокол v2: `Up` и `Down` несут instance-поколение бутылки. Номер —
//! `(boot_seed << 20) | counter`: старшие биты читаются из файла
//! `<data>/generation-seed` при старте демона (персистентные поколения,
//! T11) и различаются у любых двух запусков, младшие 20 бит — монотонный
//! счётчик стартов в пределах запуска. Агент опускает бутылку только
//! `Down` с сохранным поколением — `Down` проигравшего рестарта получает
//! `STALE_INSTANCE` и не гасит живую бутылку победителя.
//!
//! Живучесть (T11, спека §9): health-цикл ([`BottleManager::health_pass`],
//! период [`HEALTH_CHECK_PERIOD`]) опрашивает агента; смерть агента
//! переводит env-бутылки в `Degraded` (локальная метка — бутылки не
//! трогаем, WSL лежит), воскрешение — пере-поднимает их (новые порты,
//! свежий конфиг, новое поколение; призраки в аплаенсе гасит `Down`
//! старого поколения). Host-бутылки цикл проверяет по живости их
//! собственного интерфейса (рулинг префлайта: не слепой stop). Смерть
//! relay-задачи (не по остановке) помечает бутылку `Degraded{причина}`.
//! Сон/резюме хоста покрывается тем же циклом.
//!
//! Контракт конкурентности: runtime-карта живёт под одним мьютексом,
//! лок удерживается только на коротких участках без `await`; долгие
//! операции (соединение с агентом, `Up` с handshake туннеля) выполняются
//! без лока, а результат докладывается вторым коротким локом. Маркер
//! `starting` несёт номер поколения: если за время старта пришёл `stop`
//! (запись вынута) или произошёл перезапуск (запись заменена чужим
//! поколением), сделанное полностью разбирается.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bottle_core::ipc::{BottleState, CliReply, RuntimeState, VpnCredentials};
use bottle_core::profile::{BottleKind, BottleProfile, TunnelProtocol, VpnConfig};
use bottle_core::protocol::{err, AgentReply, AgentRequest, ForwardPort, UpArgs};
use relay_lib::forward::{serve_forward, TargetAddr};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;

use crate::appliance::{ensure_appliance, ApplianceHandle, WslRunner};
use crate::error::DaemonError;
use crate::host_manager::{HostManager, HostParts, HostTiming};
use crate::store::{atomic_write, Store};

/// Нижняя граница диапазона автоподбора портов (dynamic, RFC 6335).
pub const PORT_DYNAMIC_MIN: u16 = 49152;
/// Верхняя граница диапазона автоподбора портов.
pub const PORT_DYNAMIC_MAX: u16 = 65535;
/// Портов перебирается, прежде чем сдаться (прямой bind конкретных
/// номеров диапазона, со случайного старта).
const PORT_DYNAMIC_ATTEMPTS: usize = 1000;
/// Сколько [`RealAgentConnect`] ждёт соединения с агентом после wake
/// (агент стартует при первом обращении к VM; спека §4.3 — ≤30 с).
pub const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Период health-цикла демона (T11, спека §9): поллинг агента, живость
/// host-интерфейсов, разбор смертей relay-задач.
pub const HEALTH_CHECK_PERIOD: Duration = Duration::from_secs(10);
/// Имя файла персистентного поколения в корне данных демона.
const GENERATION_SEED_FILE: &str = "generation-seed";
/// Число бит счётчика стартов в младшей части instance-поколения
/// (рулинг T11: `instance = (seed << 20) | counter`, counter < 2^20).
const INSTANCE_COUNTER_BITS: u32 = 20;

/// Точка подключения к агенту аплаенса; инъекция для тестов.
///
/// Каждая операция жизненного цикла (`start`/`stop`) открывает своё
/// соединение: `connect` дешёвый (пока жива хоть одна бутылка, VM не
/// гаснет), агент обслуживает последовательных клиентов. Health-цикл
/// (T11) опрашивает живость через [`AgentConnect::alive`] тем же
/// способом — контрольное соединение с `Status`.
#[async_trait]
pub trait AgentConnect: Send + Sync {
    /// Устанавливает соединение с агентом (handshake включён).
    async fn connect(&self) -> Result<ApplianceHandle, DaemonError>;

    /// Жив ли агент: контрольное соединение и опрос `Status` без id.
    /// Используется health-циклом (T11, спека §9); `false` — умер или
    /// недоступен (WSL выключен, сон хоста, рестарт VM).
    async fn alive(&self) -> bool;
}

/// Продакшн-подключение: [`ensure_appliance`] — импорт дистрибутива,
/// провижининг токена, wake и handshake — при каждом соединении.
pub struct RealAgentConnect {
    /// Исполнитель команд WSL (`wsl.exe`).
    wsl: Arc<dyn WslRunner>,
    /// Корень данных демона (здесь лежит токен аплаенса).
    data_root: PathBuf,
    /// Тарболл rootfs для `wsl --import` свежей установки.
    rootfs_tar: PathBuf,
    /// Таймаут соединения с агентом.
    connect_timeout: Duration,
}

impl RealAgentConnect {
    /// Собирает подключение из исполнителя WSL и путей.
    pub fn new(
        wsl: Arc<dyn WslRunner>,
        data_root: PathBuf,
        rootfs_tar: PathBuf,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            wsl,
            data_root,
            rootfs_tar,
            connect_timeout,
        }
    }
}

#[async_trait]
impl AgentConnect for RealAgentConnect {
    async fn connect(&self) -> Result<ApplianceHandle, DaemonError> {
        let store = Store::open(&self.data_root)?;
        ensure_appliance(
            self.wsl.as_ref(),
            &store,
            &self.rootfs_tar,
            self.connect_timeout,
        )
        .await
    }

    async fn alive(&self) -> bool {
        match self.connect().await {
            Ok(mut handle) => handle.alive().await,
            Err(error) => {
                tracing::debug!(%error, "health-проба агента не удалась");
                false
            }
        }
    }
}

/// Текущее время в unix-миллисекундах (источник значения generation-seed).
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Вычисляет seed очередного запуска демона: текущее время; если файл
/// уже содержит не меньшее значение (два запуска в одну миллисекунду,
/// часы ушли назад) — увеличить. Любые два запуска получают разные
/// seed, значит их instance-диапазоны `(seed << 20)..(seed+1 << 20)`
/// не пересекаются.
fn next_seed(previous: Option<u64>, now_ms: u64) -> u64 {
    match previous {
        Some(prev) if prev >= now_ms => prev + 1,
        _ => now_ms,
    }
}

/// Читает/обновляет файл поколения `<data>/generation-seed` и возвращает
/// seed этого запуска (T11: персистентные поколения). Файл создаётся при
/// первом старте с текущим временем; каждый следующий старт продвигает
/// значение (см. [`next_seed`]); битое содержимое перезаписывается.
fn advance_generation_seed(data_root: &Path) -> io::Result<u64> {
    let path = data_root.join(GENERATION_SEED_FILE);
    let previous = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    let seed = next_seed(previous, unix_ms());
    atomic_write(&path, seed.to_string().as_bytes())?;
    Ok(seed)
}

/// Тайминги живучести (T11): паузы переподключения к аплаенсу после
/// неудачных health-проб; период задаётся отдельно при спавне цикла
/// (продакшн — [`HEALTH_CHECK_PERIOD`], тесты сжимают).
#[derive(Debug, Clone)]
pub struct ResilienceTiming {
    /// Паузы между попытками reconnect: берётся элемент с индексом
    /// попытки, за пределами списка — последняя (5с/15с/60с/60с…,
    /// бесконечно).
    pub reconnect_backoff: Vec<Duration>,
}

impl Default for ResilienceTiming {
    fn default() -> Self {
        Self {
            reconnect_backoff: vec![
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(60),
            ],
        }
    }
}

impl ResilienceTiming {
    /// Пауза перед повторной пробой после `attempt`-й неудачи подряд
    /// (счёт с нуля); за пределами списка — 60 с.
    fn backoff(&self, attempt: u32) -> Duration {
        self.reconnect_backoff
            .get(attempt as usize)
            .copied()
            .unwrap_or(Duration::from_secs(60))
    }
}

/// Состояние health-цикла одного менеджера (T11).
#[derive(Debug)]
struct HealthState {
    /// Агент отвечал на последней пробе. Переход `true → false`
    /// деградирует env-бутылки, `false → true` — восстанавливает.
    agent_up: bool,
    /// Число неудачных проб подряд (индекс бэкоффа reconnect).
    attempts: u32,
    /// Когда разрешена следующая проба после неудач (бэкофф).
    next_attempt: Option<tokio::time::Instant>,
    /// Id бутылок, чьё восстановление (stop → start) выполняется
    /// прямо сейчас: no-op stop пользователя в этом окне — команда
    /// «не поднимай обратно» (рулинг M3-бэклога).
    restoring: std::collections::HashSet<String>,
    /// Id, остановленные пользователем во время восстановления:
    /// restore_after_recovery видит флаг между своим stop и start и
    /// пропускает пере-подъём.
    user_abort: std::collections::HashSet<String>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            agent_up: true,
            attempts: 0,
            next_attempt: None,
            restoring: std::collections::HashSet::default(),
            user_abort: std::collections::HashSet::default(),
        }
    }
}

/// Смерть relay-задачи бутылки (T11, п.3): задача завершилась ошибкой,
/// а не по остановке — туннель жив, бутылка помечается `Degraded`.
pub(crate) struct RelayDeath {
    /// Бутылка, чей релей умер.
    pub id: String,
    /// Причина (ошибка акцепт-цикла релея с портом).
    pub reason: String,
}

/// Будущее одного сервера релея (см. [`RelayServe`]).
type ServeFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

/// Сервер одного релея env-бутылки `127.0.0.1:port → vm_ip:port`;
/// продакшн — [`serve_forward`], тест смерти relay-задачи (T11, п.3)
/// подменяет немедленно падающим сервером.
pub(crate) type RelayServe = Arc<dyn Fn(TcpListener, TargetAddr) -> ServeFuture + Send + Sync>;

/// Продакшн-сервер релея: [`serve_forward`] без обёрток.
fn real_relay_serve() -> RelayServe {
    Arc::new(|listener, target| Box::pin(serve_forward(listener, target)))
}

/// Зарезервированные слушатели бутылки на `127.0.0.1` (резерв = сам
/// слушатель: порт занят, пока слушатель жив).
///
/// Резерв хранится std-слушателями в неблокирующем режиме: задача
/// проброса получает кроссплатформенный дубликат (`try_clone`: dup на
/// unix, дубликат хендла сокета на Windows — режим сохраняется) и
/// оборачивает его в tokio через `from_std`; оригинал остаётся здесь.
pub struct HostListeners {
    /// Слушатель SOCKS5; порт — из `local_addr`.
    pub socks5: StdTcpListener,
    /// Слушатели пробросов со своими спецификациями (порт + цель).
    pub forwards: Vec<(ForwardPort, StdTcpListener)>,
}

impl HostListeners {
    /// Порт слушателя SOCKS5.
    pub fn socks5_port(&self) -> Result<u16, String> {
        self.socks5
            .local_addr()
            .map(|addr| addr.port())
            .map_err(|e| format!("порт слушателя SOCKS5 недоступен: {e}"))
    }

    /// Спецификации пробросов для `UpArgs`/`RuntimeState`
    /// (переиспользуются host-менеджером).
    pub(crate) fn forward_specs(&self) -> Vec<ForwardPort> {
        self.forwards.iter().map(|(spec, _)| spec.clone()).collect()
    }
}

/// Рантайм одной бутылки в карте менеджера.
pub struct Runtime {
    /// Текущее состояние.
    pub state: BottleState,
    /// Поколение маркера `starting`, создавшего запись. `stop` вынимает
    /// запись целиком, повторный `start` вставляет новую с новым номером —
    /// фаза 3 старта признаёт результат своим, только если поколение
    /// записи совпадает (гонка stop → restart, см. [`BottleManager::start`]).
    pub generation: u64,
    /// Деградирована health-циклом из-за смерти агента (не локальной
    /// смерти релея): такие бутылки пере-поднимаются после воскрешения
    /// агента (T11).
    pub health_degraded: bool,
    /// Зарезервированные слушатели; `None` после teardown — порты
    /// освобождены (бутылка `failed`, но видна в `status` с причиной).
    pub listeners: Option<HostListeners>,
    /// Задачи `serve_forward`; гасятся при `stop`.
    pub tasks: JoinSet<()>,
}

/// Результат успешного провижининга (фаза без лока в `start`).
struct Provisioned {
    listeners: HostListeners,
    tasks: JoinSet<()>,
}

/// Менеджер бутылок: старт/стоп/статус env- и host-бутылок (спека §7.1/§7.2).
pub struct BottleManager {
    /// Хранилище профилей (операции короткие, чтения потокобезопасны).
    store: Store,
    /// Подключение к агенту аплаенса.
    connect: Arc<dyn AgentConnect>,
    /// Менеджер host-бутылок (маршрутизация по `kind`, спека §7.2);
    /// `Arc` — фоновый цикл DNS-перепроверки держит его после старта.
    host: Arc<HostManager>,
    /// Живые (и упавшие с ошибкой) бутылки по id.
    runtime: Mutex<HashMap<String, Runtime>>,
    /// Счётчик поколений маркеров `starting` в пределах запуска —
    /// младшие биты instance (см. [`Runtime::generation`]).
    generations: AtomicU64,
    /// Seed запуска из `generation-seed` — старшие биты instance.
    boot_seed: u64,
    /// Тайминги живучести (бэкофф reconnect).
    resilience: ResilienceTiming,
    /// Состояние health-цикла.
    health: Mutex<HealthState>,
    /// Версия аплаенса из последнего успешного handshake (для сводок
    /// logs export и диагностики doctor); `None` — не подключались.
    appliance_version: Mutex<Option<String>>,
    /// Сервер одного релея (инъекция для теста смерти задачи).
    relay_serve: RelayServe,
    /// Канал смертей relay-задач env-бутылок (дренируется health-циклом).
    relay_tx: mpsc::UnboundedSender<RelayDeath>,
    relay_rx: Mutex<mpsc::UnboundedReceiver<RelayDeath>>,
}

impl BottleManager {
    /// Создаёт менеджера над хранилищем и подключением; host-часть —
    /// продакшн-инъекции ([`crate::host_manager::HostParts::real`]).
    pub fn new(store: Store, connect: Arc<dyn AgentConnect>) -> Self {
        Self::new_with_host(
            store,
            connect,
            HostManager::new(HostParts::real(), HostTiming::default()),
        )
    }

    /// Создаёт менеджера с явно переданным host-менеджером (инъекция
    /// для тестов и dev-сборок).
    pub fn new_with_host(store: Store, connect: Arc<dyn AgentConnect>, host: HostManager) -> Self {
        let boot_seed = advance_generation_seed(store.data_root()).unwrap_or_else(|error| {
            tracing::warn!(
                %error,
                "не удалось обновить файл поколения: instance без персистентности между запусками"
            );
            unix_ms()
        });
        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        Self {
            store,
            connect,
            host: Arc::new(host),
            runtime: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
            boot_seed,
            resilience: ResilienceTiming::default(),
            health: Mutex::new(HealthState::default()),
            appliance_version: Mutex::new(None),
            relay_serve: real_relay_serve(),
            relay_tx,
            relay_rx: Mutex::new(relay_rx),
        }
    }

    /// Переопределяет тайминги живучести (тесты сжимают бэкофф).
    pub fn with_resilience_timing(mut self, timing: ResilienceTiming) -> Self {
        self.resilience = timing;
        self
    }

    /// Переопределяет сервер одного релея (тест смерти relay-задачи,
    /// T11 п.3: немедленно падающий сервер вместо `serve_forward`).
    pub fn with_relay_serve(mut self, serve: RelayServe) -> Self {
        self.relay_serve = serve;
        self
    }

    /// Запускает фоновый цикл DNS-перепроверки host-бутылок (T10,
    /// спека §7.2 шаг 5): каждые 30 с активые endpoint'ы re-resolve'ятся,
    /// новые адреса получают маршруты. Задача завершается отменой
    /// `shutdown` (вместе с демоном).
    pub fn spawn_host_dns_recheck(&self, shutdown: tokio_util::sync::CancellationToken) {
        let host = Arc::clone(&self.host);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(crate::host_manager::DNS_RECHECK_STEP);
            // Долгий проход (несколько PS-запросов) не должен собирать
            // очередь пропущенных тиков — следующий через полный шаг.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("цикл DNS-перепроверки остановлен вместе с демоном");
                        break;
                    }
                    _ = ticker.tick() => host.recheck_dns().await,
                }
            }
        });
    }

    /// Запускает фоновый цикл живучести (T11, спека §9): каждый период —
    /// [`BottleManager::health_pass`]. Живёт до отмены `shutdown`
    /// (вместе с демоном); сон/резюме хоста покрывается этим же циклом
    /// (первая проба после пробуждения замечает смерть агента).
    pub fn spawn_health_loop(
        self: &Arc<Self>,
        shutdown: tokio_util::sync::CancellationToken,
        period: Duration,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            // Долгий проход (reconnect, восстановление бутылок) не должен
            // собирать очередь пропущенных тиков.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("health-цикл остановлен вместе с демоном");
                        break;
                    }
                    _ = ticker.tick() => manager.health_pass().await,
                }
            }
        });
    }

    /// Один проход цикла живучести (T11, спека §9):
    ///
    /// 1. Смерти relay-задач env- и host-бутылок → `Degraded{причина}`
    ///    (туннель жив, задача — нет; остановка сюда не попадает —
    ///    задачи гасятся abort'ом).
    /// 2. Host-бутылки: живость их интерфейса (рулинг префлайта — не
    ///    слепой stop по агенту); умерший интерфейс → полный host-stop
    ///    путь + метка `stopped`.
    /// 3. Агент: проба [`AgentConnect::alive`]; смерть → env-бутылки
    ///    `Degraded` (локальная метка, слушатели не трогаем — WSL лежит),
    ///    повторные пробы по бэкоффу; воскрешение → пере-подъём всех
    ///    бутылок, деградированных по здоровью.
    pub async fn health_pass(&self) {
        self.collect_relay_deaths().await;
        self.host.collect_relay_deaths().await;
        self.host.check_iface_alive().await;

        // Нет активных env-бутылок — проба агента не нужна: VM может
        // спать, восстанавливать некого, а alive-проба — это запуск
        // wsl.exe каждые HEALTH_CHECK_PERIOD вечно (T11-концерн,
        // M3-бэклог). Первый start разбудит аплаенс сам.
        let has_active_env = {
            let runtime = self.runtime.lock().await;
            runtime.values().any(|entry| is_active(&entry.state))
        };
        if !has_active_env {
            return;
        }

        let mut health = self.health.lock().await;
        // Бэкофф: пока аплаенс мёртв, пробы идут не чаще расписания.
        if !health.agent_up
            && health
                .next_attempt
                .is_some_and(|at| tokio::time::Instant::now() < at)
        {
            return;
        }
        if self.connect.alive().await {
            if !health.agent_up {
                health.agent_up = true;
                health.attempts = 0;
                health.next_attempt = None;
                drop(health);
                tracing::info!("агент аплаенса снова отвечает: восстановление env-бутылок");
                self.restore_after_recovery().await;
            }
        } else {
            let first_failure = health.agent_up;
            health.agent_up = false;
            let backoff = self.resilience.backoff(health.attempts);
            health.attempts += 1;
            health.next_attempt = Some(tokio::time::Instant::now() + backoff);
            drop(health);
            if first_failure {
                tracing::warn!(
                    ?backoff,
                    "агент аплаенса не отвечает: env-бутылки деградированы, повторная проба по бэкоффу"
                );
                self.degrade_env_bottles().await;
            }
        }
    }

    /// Смерть агента: env-бутылки `Up`/`Degraded` получают локальную
    /// метку `Degraded` (T11): туннель мёртв вместе с WSL, но слушатели
    /// и задачи демона не трогаем — после воскрешения они заменяются
    /// пере-подъёмом, а до него порты остаются занятыми за бутылкой.
    async fn degrade_env_bottles(&self) {
        const REASON: &str =
            "агент аплаенса не отвечает (WSL не работает?); авто-восстановление продолжается";
        let mut runtime = self.runtime.lock().await;
        for (id, entry) in runtime.iter_mut() {
            if matches!(entry.state, BottleState::Up | BottleState::Degraded { .. }) {
                entry.state = BottleState::Degraded {
                    reason: REASON.to_string(),
                };
                entry.health_degraded = true;
                tracing::warn!(id = %id, "бутылка деградирована: агент аплаенса недоступен");
            }
        }
    }

    /// Воскрешение агента: пере-подъём всех env-бутылок, деградированных
    /// health-циклом (T11). Полный путь stop → start: старые слушатели
    /// закрываются, порты резервируются заново, конфиг перечитывается,
    /// `Down` старого поколения гасит призрак в аплаенсе (если агент
    /// пережил обрыв соединения), свежий `Up` несёт новое поколение.
    /// Результат — `up` или `failed{причина}`; бутылку, остановленную
    /// пользователем за время обрыва, не трогаем.
    async fn restore_after_recovery(&self) {
        let ids: Vec<String> = {
            let runtime = self.runtime.lock().await;
            runtime
                .iter()
                .filter(|(_, entry)| {
                    entry.health_degraded
                        && matches!(entry.state, BottleState::Up | BottleState::Degraded { .. })
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            // Окно восстановления: пользовательский stop между нашим
            // stop и start — команда «не поднимай обратно» (рулинг
            // M3-бэклога): no-op stop смотрит в restoring и ставит
            // user_abort.
            self.health.lock().await.restoring.insert(id.clone());
            match self.stop(&id).await {
                Ok(CliReply::Stopped { .. }) => {
                    if self.health.lock().await.user_abort.remove(&id) {
                        tracing::info!(id = %id, "восстановление отменено пользователем");
                        self.health.lock().await.restoring.remove(&id);
                        continue;
                    }
                    if let Err(error) = self.start(&id, None).await {
                        // Профиль недоступен и т.п.: маркер failed с
                        // причиной, иначе бутылка исчезнет из status молча.
                        self.runtime.lock().await.insert(
                            id.clone(),
                            Runtime {
                                state: BottleState::Failed {
                                    reason: error.to_string(),
                                },
                                generation: 0,
                                health_degraded: false,
                                listeners: None,
                                tasks: JoinSet::new(),
                            },
                        );
                        tracing::warn!(id = %id, %error, "восстановление бутылки не удалось");
                    }
                }
                _ => tracing::info!(id = %id, "восстановление пропущено: бутылка уже остановлена"),
            }
            self.health.lock().await.restoring.remove(&id);
        }
    }

    /// Разбирает накопленные смерти relay-задач env-бутылок: живая
    /// (`up`/`degraded`) бутылка получает `Degraded{причина}` — не
    /// `failed`: туннель и остальные релеи живут (T11, п.3).
    async fn collect_relay_deaths(&self) {
        let deaths: Vec<RelayDeath> = {
            let mut rx = self.relay_rx.lock().await;
            let mut drained = Vec::new();
            while let Ok(death) = rx.try_recv() {
                drained.push(death);
            }
            drained
        };
        for death in deaths {
            let mut runtime = self.runtime.lock().await;
            match runtime.get_mut(&death.id) {
                Some(entry)
                    if matches!(entry.state, BottleState::Up | BottleState::Degraded { .. }) =>
                {
                    entry.state = BottleState::Degraded {
                        reason: death.reason.clone(),
                    };
                    tracing::warn!(id = %death.id, reason = %death.reason, "relay-задача погибла: бутылка деградирована");
                }
                // Остановленную/упавшую бутылку смерть её задачи не
                // оживляет и не перезаписывает её состояние.
                _ => {}
            }
        }
    }

    /// Поднимает бутылку: env — по спеке §7.1, host — делегируется
    /// [`HostManager`] (спека §7.2).
    ///
    /// Три фазы: (1) короткий лок — пометка `starting` со свежим номером
    /// поколения (активная бутылка — no-op по §5.2); (2) без лока — резерв
    /// портов, чтение конфига, соединение с агентом, `Up` → `Ready` и
    /// задачи проброса; (3) короткий лок — запись результата (`up`/`failed`),
    /// если поколение записи всё ещё наше; иначе (выиграл `stop` или
    /// перезапуск) — полный teardown СВОИХ ресурсов, вплоть до `Down`
    /// агенту за уже отправленный `Up`.
    ///
    /// `Err` возможен только при чтении профиля; остальные неудачи —
    /// `Ok(CliReply::Error)` с бутылкой в состоянии `failed`.
    ///
    /// `credentials` — логин/пароль VPN этой сессии (OpenVPN
    /// `auth-user-pass`). Если конфиг требует авторизацию, а кредов нет,
    /// отвечает [`CliReply::NeedCredentials`] ДО маркера `starting` —
    /// клиент спрашивает пользователя и повторяет `Start` с кредами.
    /// Креды нигде не сохраняются: уходят агенту в `Up` и забываются.
    pub async fn start(
        &self,
        id: &str,
        credentials: Option<VpnCredentials>,
    ) -> Result<CliReply, DaemonError> {
        let profile = self.store.load(id)?;
        if profile.kind == BottleKind::Host {
            // Спека §7.2 (host-бутылки) — жизненный цикл в host-менеджере.
            return self.host.start(&profile).await;
        }

        // Предпроверка кредов до маркера starting и до обращения к
        // аплаенсу: голый auth-user-pass без кредов оборачивался
        // мгновенным exit openvpn («could not read Auth username from
        // stdin») и невнятным TUNNEL_FAILED. Битый/отсутствующий конфиг
        // здесь не разбираем — его диагностирует фаза 2, как раньше.
        if credentials.is_none()
            && matches!(
                &profile.vpn,
                VpnConfig::Env {
                    protocol: TunnelProtocol::OpenVpn,
                    ..
                }
            )
        {
            if let Ok(text) = read_config(self.store.data_root(), &profile.vpn) {
                if bottle_core::profile::openvpn_config_needs_auth(&text) {
                    tracing::info!(id, "конфиг требует auth-user-pass: запрошены креды");
                    return Ok(CliReply::NeedCredentials { id: id.to_string() });
                }
            }
        }

        // Фаза 1: пометка starting; активная бутылка — no-op (RF#3).
        // Поколение = персистентный seed запуска (старшие биты) + счётчик
        // стартов (младшие): у любых двух запусков демона множества
        // поколений не пересекаются (T11).
        let counter = self.generations.fetch_add(1, Ordering::Relaxed);
        debug_assert!(
            counter < (1 << INSTANCE_COUNTER_BITS),
            "счётчик поколений вышел за младшие {INSTANCE_COUNTER_BITS} бит"
        );
        let generation = (self.boot_seed << INSTANCE_COUNTER_BITS) | counter;
        {
            let mut runtime = self.runtime.lock().await;
            if let Some(existing) = runtime.get(id) {
                if is_active(&existing.state) {
                    tracing::warn!(
                        id,
                        state = existing.state.as_str(),
                        "бутылка уже активна: повторный start без действия"
                    );
                    return Ok(CliReply::Started { id: id.to_string() });
                }
            }
            runtime.insert(
                id.to_string(),
                Runtime {
                    state: BottleState::Starting,
                    generation,
                    health_degraded: false,
                    listeners: None,
                    tasks: JoinSet::new(),
                },
            );
        }

        // Фаза 2: длинные операции без лока; instance-поколение этого
        // старта сопровождает Up (агент сохранит его с бутылкой).
        let outcome = self.provision(&profile, credentials, generation).await;

        // Фаза 3: короткий лок. Маркер жив, только если в карте лежит
        // запись нашего поколения: stop вынимает запись, перезапуск
        // кладёт новую с другим номером; чужую запись не трогаем.
        let mut runtime = self.runtime.lock().await;
        let marker_alive = runtime.get(id).is_some_and(|r| r.generation == generation);
        match outcome {
            Ok(made) if marker_alive => {
                runtime.insert(
                    id.to_string(),
                    Runtime {
                        state: BottleState::Up,
                        generation,
                        health_degraded: false,
                        listeners: Some(made.listeners),
                        tasks: made.tasks,
                    },
                );
                drop(runtime);
                tracing::info!(id, "бутылка поднята");
                if let Some(command) = &profile.expose.launch {
                    crate::launch::spawn_launch(command);
                }
                Ok(CliReply::Started { id: id.to_string() })
            }
            Ok(mut made) => {
                // Гонку выиграл stop (записи нет) или перезапуск (в карте
                // запись чужого, более нового поколения): разобрать всё
                // сделанное ЭТИМ запуском — задачи, слушатели — и дождаться
                // освобождения портов. `Down` отправляем ВСЕГДА с
                // поколением этого старта: агент опускает только совпавшее
                // поколение, чужое отвергается STALE_INSTANCE (трактуется
                // как успех) — бутылке победителя он не вредит. Пропуск
                // Down после перезапуска оставлял бы в аплаенсе
                // бутылку-призрак проигравшего старта, навсегда
                // блокирующую id (`ALREADY_UP`) — Finding 2 ревью.
                drop(runtime);
                made.tasks.abort_all();
                while made.tasks.join_next().await.is_some() {
                    // Результат (в т.ч. cancellation) не важен — ждём
                    // завершения, чтобы порты освободились до ответа.
                }
                drop(made.listeners);
                if let Err(error) = self.send_down(id, generation).await {
                    tracing::warn!(
                        id,
                        %error,
                        "не удалось опустить бутылку в аплаенсе после гонки stop/start"
                    );
                }
                Ok(CliReply::Error {
                    message: "бутылка остановлена во время запуска".to_string(),
                })
            }
            Err(reason) => {
                if marker_alive {
                    runtime.insert(
                        id.to_string(),
                        Runtime {
                            state: BottleState::Failed {
                                reason: reason.clone(),
                            },
                            generation,
                            health_degraded: false,
                            listeners: None,
                            tasks: JoinSet::new(),
                        },
                    );
                }
                drop(runtime);
                tracing::warn!(id, %reason, "поднятие бутылки не удалось");
                Ok(CliReply::Error { message: reason })
            }
        }
    }

    /// Опускает бутылку: env — `Down` агенту, задачи и слушатели — вниз;
    /// host — делегируется [`HostManager`] (спека §7.2, шаг 6).
    ///
    /// Ошибки агента не мешают локальному teardown (агент мог упасть);
    /// не запущенная бутылка — понятная ошибка, не сбой.
    pub async fn stop(&self, id: &str) -> Result<CliReply, DaemonError> {
        // Маршрутизация. Сначала runtime-состояние: host-сессия могла
        // остаться при удалённом/битом профиле, и её нужно разобрать
        // (T11 carried minor из T9/T10 — не отвечать «не запущена» на
        // живую сессию); затем kind профиля.
        if self.host.has_active_session(id).await {
            return self.host.stop(id).await;
        }
        if let Ok(profile) = self.store.load(id) {
            if profile.kind == BottleKind::Host {
                return self.host.stop(id).await;
            }
        }
        // Короткий лок: активную бутылку вынуть из карты целиком.
        let runtime = {
            let mut runtime = self.runtime.lock().await;
            if runtime.get(id).is_some_and(|r| is_active(&r.state)) {
                runtime.remove(id)
            } else {
                None
            }
        };
        let Some(mut runtime) = runtime else {
            // Повторный stop уже остановленной — no-op с
            // предупреждением, не ошибка (спека §5.2). Исключение —
            // окно восстановления: health уже снял бутылку своим
            // stop, start ещё не начался; пользовательский stop здесь
            // означает «не поднимай обратно» (рулинг M3-бэклога).
            {
                let mut health = self.health.lock().await;
                if health.restoring.contains(id) {
                    health.user_abort.insert(id.to_string());
                    tracing::info!(id, "пользовательский stop в окне восстановления");
                }
            }
            return Ok(CliReply::Stopped {
                id: id.to_string(),
                warning: Some("не была запущена".to_string()),
            });
        };
        runtime.state = BottleState::Stopping;

        // Down несёт поколение вынутой записи — опускаем именно свою
        // бутылку (чужую агент отвергнет STALE_INSTANCE).
        if let Err(error) = self.send_down(id, runtime.generation).await {
            tracing::warn!(id, %error, "агенту не удалось отправить Down; продолжаю остановку");
        }

        // Погасить задачи (join — чтобы порты гарантированно освободились
        // до ответа) и закрыть слушатели.
        runtime.tasks.abort_all();
        while runtime.tasks.join_next().await.is_some() {
            // Результат (в т.ч. cancellation) не важен — ждём завершения.
        }
        runtime.listeners = None;
        tracing::info!(id, "бутылка остановлена");
        Ok(CliReply::Stopped {
            id: id.to_string(),
            warning: None,
        })
    }

    /// Рантайм-состояние всех бутылок (включая `failed` и host-сессии),
    /// по id.
    pub async fn status(&self) -> Vec<RuntimeState> {
        let runtime = self.runtime.lock().await;
        let mut bottles: Vec<RuntimeState> = runtime
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
            .collect();
        drop(runtime);
        bottles.extend(self.host.status().await);
        bottles.sort_by(|a, b| a.id.cmp(&b.id));
        bottles
    }

    /// Версия аплаенса из последнего успешного handshake;
    /// `None` — в этой сессии аплаенс не подключался.
    pub async fn appliance_version(&self) -> Option<String> {
        self.appliance_version.lock().await.clone()
    }

    /// Запоминает версию аплаенса из свежего handshake (сводки
    /// `logs export`, диагностика `doctor`, ответ `upgrade`).
    pub async fn remember_appliance_version(&self, version: &str) {
        *self.appliance_version.lock().await = Some(version.to_string());
    }

    /// Фаза 2 `start`: резерв портов, чтение конфига, `Up` → `Ready`,
    /// задачи проброса. При ошибке всё сделанное освобождается
    /// (резерв-слушатели дропаются — порты свободны). `instance` —
    /// поколение этого старта, оно сопровождает `Up` и компенсирующий
    /// `Down` (агент хранит его с бутылкой).
    async fn provision(
        &self,
        profile: &BottleProfile,
        credentials: Option<VpnCredentials>,
        instance: u64,
    ) -> Result<Provisioned, String> {
        let VpnConfig::Env { protocol, .. } = &profile.vpn else {
            return Err("конфиг есть только у env-бутылок".to_string()); // start() уже отфильтровал
        };
        let protocol = *protocol;

        let selection = reserve_ports(profile)?;
        let config = read_config(self.store.data_root(), &profile.vpn)?;

        let mut handle = self
            .connect
            .connect()
            .await
            .map_err(|error| format!("аплаенс недоступен: {error}"))?;
        let vm_ip = handle.vm_ip;
        self.remember_appliance_version(&handle.appliance_version)
            .await;

        // Up; PORT_BUSY → перевыбор динамических портов (фиксированные
        // бинды сохраняются — их порт изменить нельзя) и повтор ровно
        // один раз (RF#5). Повтор — то же поколение: instance у обеих
        // попыток одинаков. Креды уходят в оба Up.
        let selection = match handle
            .send(up_args(
                profile,
                protocol,
                config.clone(),
                &selection,
                credentials.clone(),
                instance,
            )?)
            .await
        {
            Ok(AgentReply::Ready { .. }) => selection,
            Ok(AgentReply::Error { code, .. }) if code == err::PORT_BUSY => {
                tracing::warn!(id = %profile.id, "агент сообщил PORT_BUSY: перевыбор портов");
                let retry = retry_ports(profile, selection)?;
                match handle
                    .send(up_args(
                        profile,
                        protocol,
                        config,
                        &retry,
                        credentials,
                        instance,
                    )?)
                    .await
                {
                    Ok(AgentReply::Ready { .. }) => retry,
                    Ok(AgentReply::Error { code, message }) if code == err::PORT_BUSY => {
                        return Err(port_busy_after_retry(profile, &message))
                    }
                    Ok(AgentReply::Error { code, message }) => {
                        return Err(agent_error(code, message))
                    }
                    Ok(other) => return Err(unexpected_reply(&other)),
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok(AgentReply::Error { code, message }) => return Err(agent_error(code, message)),
            Ok(other) => return Err(unexpected_reply(&other)),
            Err(error) => return Err(error.to_string()),
        };

        // Ready: relay агента уже слушает vm_ip:port — перестроить резерв
        // на serve_forward(127.0.0.1:port → vm_ip:port). Задачи получают
        // клоны слушателей; оригиналы остаются в рантайме бутылки.
        // Если задачи не поднялись, бутылка уже создана в аплаенсе —
        // опустить её (`Down` за наш `Up`) и вернуть ошибку.
        let mut tasks = JoinSet::new();
        let spawned = spawn_forwards(
            &mut tasks,
            &selection,
            vm_ip,
            &profile.id,
            Arc::clone(&self.relay_serve),
            self.relay_tx.clone(),
        );
        if let Err(reason) = spawned {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            drop(selection);
            if let Err(error) = handle
                .send(AgentRequest::Down {
                    id: profile.id.clone(),
                    instance,
                })
                .await
            {
                tracing::warn!(id = %profile.id, %error, "Down после неудачного старта не дошёл");
            }
            return Err(reason);
        }
        Ok(Provisioned {
            listeners: selection,
            tasks,
        })
    }

    /// Отправляет агенту `Down` с instance-поколением бутылки.
    ///
    /// `NOT_FOUND` (бутылки в аплаенсе уже нет) и `STALE_INSTANCE`
    /// (под этим id живёт бутылка чужого, более нового поколения —
    /// выиграл рестарт; своя уже заменена) считаются успехом: в обоих
    /// случаях опускать нечего, защита живой бутылки сработала.
    async fn send_down(&self, id: &str, instance: u64) -> Result<(), DaemonError> {
        let mut handle = self.connect.connect().await?;
        self.remember_appliance_version(&handle.appliance_version)
            .await;
        let request = AgentRequest::Down {
            id: id.to_string(),
            instance,
        };
        match handle.send(request).await {
            Ok(AgentReply::Ok) => Ok(()),
            Ok(AgentReply::Error { code, .. }) if code == err::NOT_FOUND => Ok(()),
            Ok(AgentReply::Error { code, .. }) if code == err::STALE_INSTANCE => {
                tracing::info!(
                    id,
                    instance,
                    "агент сохранил бутылку чужого поколения (STALE_INSTANCE)"
                );
                Ok(())
            }
            Ok(AgentReply::Error { code, message }) => Err(DaemonError::Appliance(format!(
                "агент отверг Down ({code}): {message}"
            ))),
            Ok(other) => Err(DaemonError::Appliance(format!(
                "неожиданный ответ агента на Down: {other:?}"
            ))),
            Err(error) => Err(error),
        }
    }
}

/// Активные состояния: повторный start — no-op, stop — вынимает запись.
fn is_active(state: &BottleState) -> bool {
    matches!(
        state,
        BottleState::Starting
            | BottleState::Up
            | BottleState::Degraded { .. }
            | BottleState::Stopping
    )
}

/// Резервирует слушатели портов: автоподбор из 49152–65535 для нулевых
/// портов, точный bind для фиксированных (занятый порт — понятная ошибка).
/// Резерв — сам слушатель: порт занят, пока слушатель жив.
/// Переиспользуется host-менеджером (релеи host-бутылок на 127.0.0.1).
pub(crate) fn reserve_ports(profile: &BottleProfile) -> Result<HostListeners, String> {
    let socks5 = match profile.expose.socks5 {
        0 => reserve_dynamic_port()?,
        port => bind_reserved(port, "SOCKS5")?,
    };
    let mut forwards = Vec::with_capacity(profile.expose.forwards.len());
    for spec in &profile.expose.forwards {
        let listener = match spec.listen {
            0 => reserve_dynamic_port()?,
            port => bind_reserved(port, &format!("проброс на {}", spec.target))?,
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        forwards.push((
            ForwardPort {
                port,
                target: spec.target.clone(),
            },
            listener,
        ));
    }
    Ok(HostListeners { socks5, forwards })
}

/// Пересобирает резерв портов для повтора `Up` после PORT_BUSY:
/// фиксированные бинды переходят как есть (порт профиля изменить
/// нельзя — и повторный bind того же порта не нужен), перевыбираются
/// только динамические порты (новый занимается ДО освобождения
/// старого, поэтому гарантированно отличается от него).
fn retry_ports(profile: &BottleProfile, previous: HostListeners) -> Result<HostListeners, String> {
    let socks5 = if profile.expose.socks5 == 0 {
        let fresh = reserve_dynamic_port()?;
        drop(previous.socks5);
        fresh
    } else {
        previous.socks5
    };
    let mut forwards = Vec::with_capacity(profile.expose.forwards.len());
    for (spec, (_, listener)) in profile.expose.forwards.iter().zip(previous.forwards) {
        let listener = if spec.listen == 0 {
            let fresh = reserve_dynamic_port()?;
            drop(listener);
            fresh
        } else {
            listener
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        forwards.push((
            ForwardPort {
                port,
                target: spec.target.clone(),
            },
            listener,
        ));
    }
    Ok(HostListeners { socks5, forwards })
}

/// Причина неудачи, когда и повторный `Up` получил PORT_BUSY.
///
/// Если в профиле есть фиксированные порты, повтор их не меняет —
/// конфликт на стороне агента снимается только правкой профиля или
/// освобождением порта в аплаенсе; говорим об этом прямо, а не
/// маскируем кодом агента.
fn port_busy_after_retry(profile: &BottleProfile, agent_message: &str) -> String {
    let fixed: Vec<String> = [profile.expose.socks5]
        .into_iter()
        .chain(profile.expose.forwards.iter().map(|f| f.listen))
        .filter(|port| *port != 0)
        .map(|port| port.to_string())
        .collect();
    if fixed.is_empty() {
        format!("{}: {agent_message}", err::PORT_BUSY)
    } else {
        format!(
            "{}: {agent_message}; порт(ы) {} занят(ы) на стороне агента и не меняются \
             повтором — измените socks5/listen в профиле или освободите порт в аплаенсе",
            err::PORT_BUSY,
            fixed.join(", ")
        )
    }
}

/// Занимает свободный порт динамического диапазона: прямой перебор
/// 49152–65535 со случайного старта, `bind` конкретного номера.
/// Эфемерный `bind :0` не используем: на Linux-хостах его диапазон
/// (`ip_local_port_range`, обычно 32768–60999) может не пересекаться
/// с нашим окном — на CI-раннерах лотерея :0 исчерпывала все попытки.
fn reserve_dynamic_port() -> Result<StdTcpListener, String> {
    let span = u32::from(PORT_DYNAMIC_MAX - PORT_DYNAMIC_MIN) + 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let start = (nanos ^ std::process::id()) % span;
    for i in 0..PORT_DYNAMIC_ATTEMPTS {
        let port = PORT_DYNAMIC_MIN + ((start + i as u32) % span) as u16;
        match StdTcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                set_nonblocking(&listener, port)?;
                return Ok(listener);
            }
            Err(_) => continue, // порт занят — следующий номер
        }
    }
    Err(format!(
        "не удалось подобрать порт в диапазоне {PORT_DYNAMIC_MIN}–{PORT_DYNAMIC_MAX}"
    ))
}

/// Биндит конкретный порт на `127.0.0.1` (занятый — понятная ошибка
/// с назначением порта) и переводит слушателя в неблокирующий режим.
fn bind_reserved(port: u16, purpose: &str) -> Result<StdTcpListener, String> {
    let listener = StdTcpListener::bind(("127.0.0.1", port))
        .map_err(|_| format!("порт 127.0.0.1:{port} занят ({purpose})"))?;
    set_nonblocking(&listener, port)?;
    Ok(listener)
}

/// Переводит зарезервированного слушателя в неблокирующий режим:
/// дубликаты наследуют режим (того же сокета/open file description),
/// а `TcpListener::from_std` требует неблокирующий сокет.
fn set_nonblocking(listener: &StdTcpListener, port: u16) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("порт {port}: не удалось включить неблокирующий режим: {e}"))
}

/// Читает содержимое конфига туннеля из хранилища (`bottles/<id>.conf`):
/// агент получает текст, а не имя файла. Имя из профиля проходит барьер
/// [`is_plain_config_filename`] ДО обращения к файловой системе.
fn read_config(data_root: &Path, vpn: &VpnConfig) -> Result<String, String> {
    let VpnConfig::Env { config, .. } = vpn else {
        return Err("конфиг есть только у env-бутылок".to_string());
    };
    if !is_plain_config_filename(config) {
        return Err(format!(
            "недопустимое имя файла конфига в профиле: {config:?} \
             (ожидается плоское имя из A-Z a-z 0-9 . _ -, например office-a.conf)"
        ));
    }
    let path = data_root.join("bottles").join(config);
    std::fs::read_to_string(&path)
        .map_err(|error| format!("не удалось прочитать конфиг {}: {error}", path.display()))
}

/// Плоское имя файла конфига `^[A-Za-z0-9._-]+$` без разделителей пути.
///
/// `config` приходит из профиля на диске — рукописного toml, а не только
/// из [`Store::save`] (который нормализует его в `<id>.conf`): без
/// барьера `../../etc/passwd` читал бы произвольные файлы хоста и
/// отправлял их содержимое агенту аплаенса. `.` и `..` проходят
/// посимвольный фильтр, но отвергаются явно — это traversal-намерение
/// (обе ссылки резолвятся в каталоги и чтением не дырявят, но барьер
/// не должен зависеть от этого нюанса).
fn is_plain_config_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Собирает запрос `Up` из профиля, выбранного резерва портов и
/// instance-поколения старта (агент сохранит его с бутылкой).
fn up_args(
    profile: &BottleProfile,
    protocol: TunnelProtocol,
    config: String,
    selection: &HostListeners,
    credentials: Option<VpnCredentials>,
    instance: u64,
) -> Result<AgentRequest, String> {
    let (username, password) = credentials
        .map(|c| (Some(c.username), Some(c.password)))
        .unwrap_or((None, None));
    Ok(AgentRequest::Up(UpArgs {
        id: profile.id.clone(),
        protocol,
        config,
        dns: profile.expose.dns,
        socks5_port: selection.socks5_port()?,
        forwards: selection.forward_specs(),
        instance,
        username,
        password,
    }))
}

/// Поднимает задачи проброса для всего резерва: SOCKS5 + пробросы,
/// каждый `127.0.0.1:port → vm_ip:port` (relay агента слушает те же
/// порты на VM). Смерть задачи (не по остановке) докладывается в
/// `reporter` — health-цикл пометит бутылку `Degraded` (T11, п.3).
fn spawn_forwards(
    tasks: &mut JoinSet<()>,
    selection: &HostListeners,
    vm_ip: Ipv4Addr,
    id: &str,
    serve: RelayServe,
    reporter: mpsc::UnboundedSender<RelayDeath>,
) -> Result<(), String> {
    spawn_forward(
        tasks,
        &selection.socks5,
        vm_ip,
        selection.socks5_port()?,
        id,
        Arc::clone(&serve),
        reporter.clone(),
    )?;
    for (spec, listener) in &selection.forwards {
        spawn_forward(
            tasks,
            listener,
            vm_ip,
            spec.port,
            id,
            Arc::clone(&serve),
            reporter.clone(),
        )?;
    }
    Ok(())
}

/// Ставит слушателя на проброс `127.0.0.1:port → vm_ip:port`
/// (relay агента слушает тот же порт на VM). Ошибка сервера проброса
/// (акцепт-цикл слушателя) уходит репортёру — задача умерла, но не по
/// остановке (остановка гасит задачи abort'ом без доклада).
#[allow(clippy::too_many_arguments)]
fn spawn_forward(
    tasks: &mut JoinSet<()>,
    listener: &StdTcpListener,
    vm_ip: Ipv4Addr,
    port: u16,
    id: &str,
    serve: RelayServe,
    reporter: mpsc::UnboundedSender<RelayDeath>,
) -> Result<(), String> {
    // tokio TcpListener не Clone (AsFd у него только на unix): задача
    // проброса работает на кроссплатформенном дубликате сокета —
    // try_clone сохраняет неблокирующий режим, оригинал остаётся
    // в рантайме бутылки; порт занят, пока жив хоть один из них.
    let dup = listener
        .try_clone()
        .and_then(|dup| TcpListener::from_std(dup))
        .map_err(|e| format!("не удалось клонировать слушателя порта {port}: {e}"))?;
    let target = TargetAddr::Ip(SocketAddr::new(IpAddr::V4(vm_ip), port));
    let id = id.to_string();
    tasks.spawn(async move {
        if let Err(error) = serve(dup, target).await {
            tracing::warn!(port, %error, "сервер проброса остановлен");
            let _ = reporter.send(RelayDeath {
                id,
                reason: format!("релей 127.0.0.1:{port} остановлен: {error}"),
            });
        }
    });
    Ok(())
}

/// Ошибка агента в виде причины неудачи старта.
fn agent_error(code: String, message: String) -> String {
    format!("{code}: {message}")
}

/// Неожиданный (не ошибочный) ответ агента на `Up`.
fn unexpected_reply(reply: &AgentReply) -> String {
    format!("неожиданный ответ агента на Up: {reply:?}")
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use bottle_core::frame::{decode_frame, encode_frame, LengthJsonCodec};
    use bottle_core::ipc::{BottleState, CliReply};
    use bottle_core::profile::{BottleKind, ExposeConfig, ForwardSpec, TunnelProtocol, VpnConfig};
    use bottle_core::protocol::{err, AgentReply, AgentRequest, UpArgs, PROTOCOL_VERSION};
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::tcp::OwnedWriteHalf;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Encoder, FramedRead};

    use super::*;
    use crate::error::DaemonError;

    /// env-профиль с автоподбором портов (конфиг — путь до save).
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
                launch: None,
            },
        }
    }

    /// Host-профиль (M1 его не поднимает).
    fn host_profile(id: &str) -> BottleProfile {
        BottleProfile {
            id: id.to_string(),
            name: format!("Хост {id}"),
            kind: BottleKind::Host,
            vpn: VpnConfig::Host {
                client: r"C:\Program Files\Vendor\vpn.exe".to_string(),
                args: Some("connect office".to_string()),
                endpoints: vec!["vpn.example.com".to_string()],
                interface: None,
            },
            expose: ExposeConfig {
                socks5: 0,
                forwards: Vec::new(),
                dns: None,
                launch: None,
            },
        }
    }

    /// Хранилище с одним сохранённым профилем (save копирует конфиг
    /// в `bottles/<id>.conf` содержимым `wg-office-a`; для env-профиля
    /// путь исходника в fixture подменяется на реально созданный файл —
    /// `Store::save` читает конфиг именно как путь до исходника).
    fn store_with(profile: &BottleProfile) -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("wg-office.conf");
        std::fs::write(&src, "wg-office-a").unwrap();
        let mut profile = profile.clone();
        if let VpnConfig::Env { config, .. } = &mut profile.vpn {
            *config = src.to_string_lossy().into_owned();
        }
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store.save(&profile).unwrap();
        (tmp, store)
    }

    /// Свободный порт на `127.0.0.1` (bind `:0` → номер → drop). Крошечное
    /// окно между drop и использованием допускается — тот же паттерн, что
    /// в проверках освобождения портов.
    fn free_host_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        let port = probe.local_addr().expect("local_addr").port();
        drop(probe);
        port
    }

    /// Поведение фейк-агента на очередной `Up`.
    #[derive(Clone)]
    enum UpBehavior {
        /// Всегда `Ready` (подняв relay на «vm_ip»).
        Ready,
        /// Первый `Up` — `Error{PORT_BUSY}`, дальше `Ready`.
        PortBusyOnce,
        /// Всегда `Error{PORT_BUSY}` (конфликт не устраняется повтором).
        PortBusyAlways,
        /// Всегда `Error{TUNNEL_FAILED}` с сообщением.
        TunnelFailed(String),
        /// Первый `Up` отвечает `Ready` с задержкой (моделирует долгий
        /// handshake туннеля для гонок stop/restart во время старта),
        /// остальные — сразу.
        DelayedFirstUp(Duration),
        /// Гонка «бутылка-призрак» (Finding 2 ревью): `Up`-ы
        /// обслуживаются строго по очереди (как сессии настоящего
        /// агента) через общий гейт, первый — `Ready` с задержкой
        /// (долгий handshake), а `Up` для id, уже зарегистрированного
        /// в live-реестре, — `Error{ALREADY_UP}` (барьер настоящего
        /// агента: поколение живой бутылки не перезаписывается).
        /// Поздний `Ready` проигравшего регистрирует его бутылку
        /// раньше, чем `Up` победителя дойдёт до барьера.
        DelayedFirstUpBarrier(Duration),
    }

    /// Релеи фейк-агента по (id, instance-поколению) `Up`.
    type FakeRelays = HashMap<(String, u64), Vec<tokio::task::JoinHandle<()>>>;

    /// Журнал фейк-агента: полученные `Up`/`Down` и живые relay.
    #[derive(Clone, Default)]
    struct AgentJournal {
        ups: Arc<Mutex<Vec<UpArgs>>>,
        downs: Arc<Mutex<Vec<(String, u64)>>>,
        /// Релеи по (id, instance-поколению) — как handle живой бутылки
        /// настоящего агента: `Down` совпавшего поколения разбирает
        /// релеи только этой бутылки (накапливаются, а не заменяются:
        /// при гонке рестарта поздний `Ready` проигравшего не должен
        /// гасить релеи живого победителя).
        relays: Arc<Mutex<FakeRelays>>,
        /// Живые бутылки по протоколу v2: id → instance-поколение `Up`,
        /// создавшего бутылку. Регистрируются при ответе `Ready` (как
        /// в настоящем агенте — реестр до ответа), `Down` проверяет
        /// поколение: чужое → `STALE_INSTANCE`, бутылка не трогается.
        live: Arc<Mutex<HashMap<String, u64>>>,
        /// Гейт сериализации `Up`-ов (модель последовательного агента
        /// для [`UpBehavior::DelayedFirstUpBarrier`]).
        up_gate: Arc<tokio::sync::Mutex<()>>,
        /// Счётчик alive-проб демона (`Status` без id) — пин
        /// «не будить WSL без активных бутылок» (M3-бэклог).
        probes: Arc<std::sync::atomic::AtomicUsize>,
        /// Гейт ответа на `Down`: тест держит лок — агент паркуется
        /// ДО ответа (таймлайн «пользовательский stop между stop и
        /// start восстановления», M3-бэклог).
        down_gate: Arc<tokio::sync::Mutex<()>>,
    }

    impl AgentJournal {
        fn ups(&self) -> Vec<UpArgs> {
            self.ups.lock().unwrap().clone()
        }

        /// Число alive-проб (`Status` без id) с момента сброса.
        fn probes(&self) -> usize {
            self.probes.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn reset_probes(&self) {
            self.probes.store(0, std::sync::atomic::Ordering::SeqCst)
        }

        fn downs(&self) -> Vec<(String, u64)> {
            self.downs.lock().unwrap().clone()
        }

        fn remember_relays(
            &self,
            id: &str,
            instance: u64,
            handles: Vec<tokio::task::JoinHandle<()>>,
        ) {
            self.relays
                .lock()
                .unwrap()
                .entry((id.to_string(), instance))
                .or_default()
                .extend(handles);
        }

        fn drop_relays(&self, id: &str, instance: u64) {
            if let Some(handles) = self
                .relays
                .lock()
                .unwrap()
                .remove(&(id.to_string(), instance))
            {
                for handle in handles {
                    handle.abort();
                }
            }
        }

        /// «Смерть VM»: гасит ВСЕ релеи и реестр бутылок — модель
        /// рестарта аплаенса для таймлайнов живучести (T11).
        fn drop_everything(&self) {
            let mut relays = self.relays.lock().unwrap();
            for handles in relays.values_mut() {
                for handle in handles.iter() {
                    handle.abort();
                }
            }
            relays.clear();
            self.live.lock().unwrap().clear();
        }
    }

    /// In-process фейк-агент: control-сервер на кадрах `bottle_core::frame`
    /// (последовательные соединения демона), после `Ready` поднимает relay
    /// на «vm_ip»: SOCKS5-эхо на socks5-порту, обычное эхо на портах
    /// пробросов. Протокол v2: помнит instance-поколение каждого `Up`
    /// (журнал `live`), `Down` чужого поколения → `STALE_INSTANCE`
    /// без разбора relay.
    ///
    /// От настоящего агента отличается конкурентностью: сессии
    /// обслуживаются параллельно (настоящий — по одной), поэтому при
    /// гонке наложившихся `Up` поздний `Ready` проигравшего перезапишет
    /// поколение в `live` — тестам менеджера это не важно, релеи
    /// накапливаются по поколениям и гасятся только `Down` своего
    /// поколения. Последовательность `Up`-ов и барьер живого id
    /// (как в настоящем агенте) моделирует
    /// [`UpBehavior::DelayedFirstUpBarrier`].
    ///
    /// «vm_ip» — реальный локальный IPv4 хоста: адреса 127.0.0.2+ (весь
    /// 127/8) доступны для bind только на Linux, macOS/Windows требуют
    /// явного алиаса на loopback-интерфейсе. LAN-адрес не совпадает
    /// с 127.0.0.1, на котором демон держит свои слушатели, поэтому
    /// `serve_forward` не зацикливается сам на себе; параллельные тесты
    /// не конфликтуют relay-портами: порт резервирует демон на 127.0.0.1,
    /// и он уникален в пределах процесса.
    #[derive(Clone)]
    struct FakeAgent {
        addr: SocketAddr,
        journal: AgentJournal,
        /// Жив ли агент (переключатель таймлайнов живучести T11):
        /// «мёртвый» агент не обслуживает соединения, его релеи и реестр
        /// бутылок снесены (рестарт VM).
        alive: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeAgent {
        async fn spawn(behavior: UpBehavior) -> FakeAgent {
            let vm_ip = local_ipv4();
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind control");
            let addr = listener.local_addr().expect("local_addr");
            let journal = AgentJournal::default();
            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
            tokio::spawn(run_agent(
                listener,
                vm_ip,
                behavior,
                journal.clone(),
                Arc::clone(&alive),
            ));
            FakeAgent {
                addr,
                journal,
                alive,
            }
        }

        /// Убивает агента: соединения отвергаются, релеи и реестр
        /// бутылок сносятся (модель `wsl --shutdown`).
        fn kill(&self) {
            self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
            self.journal.drop_everything();
        }

        /// Воскрешает агента: свежий реестр (REST-арт VM).
        fn resurrect(&self) {
            self.alive.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn is_alive(&self) -> bool {
            self.alive.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Локальный IPv4 хоста (адрес интерфейса маршрута по умолчанию).
    ///
    /// UDP-`connect` ничего не отправляет — ядро лишь выбирает интерфейс
    /// по таблице маршрутизации, после чего `local_addr` даёт его адрес.
    fn local_ipv4() -> Ipv4Addr {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").expect("udp bind");
        probe
            .connect("8.8.8.8:80")
            .expect("маршрут по умолчанию есть в тестовом окружении");
        match probe.local_addr().expect("udp local_addr") {
            SocketAddr::V4(addr) => *addr.ip(),
            SocketAddr::V6(addr) => panic!("маршрут по умолчанию без IPv4: {addr}"),
        }
    }

    #[async_trait]
    impl AgentConnect for FakeAgent {
        async fn connect(&self) -> Result<ApplianceHandle, DaemonError> {
            if !self.is_alive() {
                return Err(DaemonError::Appliance(
                    "фейк-агент недоступен (умер по сценарию теста)".to_string(),
                ));
            }
            let mut stream = TcpStream::connect(self.addr)
                .await
                .map_err(|e| DaemonError::Appliance(format!("фейк-агент недоступен: {e}")))?;
            stream.set_nodelay(true).expect("nodelay");
            let vm_ip = client_handshake(&mut stream).await?;
            Ok(ApplianceHandle::wrap(vm_ip, stream))
        }

        async fn alive(&self) -> bool {
            match self.connect().await {
                Ok(mut handle) => handle.alive().await,
                Err(_) => false,
            }
        }
    }

    /// `Hello` → `HelloAck` на «сыром» потоке; возвращает vm_ip из ответа.
    /// Обрыв до ответа — ошибка, не паника: агент может умереть между
    /// принятием соединения и handshake (таймлайны живучести T11) —
    /// демону это обычный «аплаенс недоступен».
    async fn client_handshake(stream: &mut TcpStream) -> Result<Ipv4Addr, DaemonError> {
        let hello = AgentRequest::Hello {
            version: PROTOCOL_VERSION,
            token: "test-token".to_string(),
        };
        stream
            .write_all(&encode_frame(&hello))
            .await
            .map_err(|e| DaemonError::Appliance(format!("фейк-агент: отправка Hello: {e}")))?;

        let mut buf = BytesMut::new();
        loop {
            if let Some(reply) = decode_frame(&mut buf).expect("кадр HelloAck") {
                let AgentReply::HelloAck { vm_ip, .. } = reply else {
                    return Err(DaemonError::Appliance(format!(
                        "ожидали HelloAck, получили {reply:?}"
                    )));
                };
                return Ok(vm_ip);
            }
            let n = stream
                .read_buf(&mut buf)
                .await
                .map_err(|e| DaemonError::Appliance(format!("фейк-агент: чтение HelloAck: {e}")))?;
            if n == 0 {
                return Err(DaemonError::Appliance(
                    "фейк-агент закрыл соединение до HelloAck (умер по сценарию)".to_string(),
                ));
            }
        }
    }

    /// Принимает соединения демона; сессии обслуживаются конкурентно.
    /// Мёртвый агент (T11) закрывает соединения без handshake.
    async fn run_agent(
        listener: TcpListener,
        vm_ip: Ipv4Addr,
        behavior: UpBehavior,
        journal: AgentJournal,
        alive: Arc<std::sync::atomic::AtomicBool>,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            if !alive.load(std::sync::atomic::Ordering::SeqCst) {
                drop(stream); // соединение встречено мёртвым агентом
                continue;
            }
            let behavior = behavior.clone();
            let journal = journal.clone();
            tokio::spawn(async move {
                serve_control_session(stream, vm_ip, behavior, journal).await;
            });
        }
    }

    /// Одна control-сессия: handshake, затем request-response.
    async fn serve_control_session(
        stream: TcpStream,
        vm_ip: Ipv4Addr,
        behavior: UpBehavior,
        journal: AgentJournal,
    ) {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FramedRead::new(read_half, LengthJsonCodec::<AgentRequest>::new());
        let mut codec = LengthJsonCodec::<AgentReply>::new();

        let Some(Ok(AgentRequest::Hello { version, .. })) = reader.next().await else {
            return; // демону нечего делать без handshake
        };
        assert_eq!(version, PROTOCOL_VERSION, "демон шлёт текущую версию");
        send_frame(
            &mut write_half,
            &mut codec,
            AgentReply::HelloAck {
                version: PROTOCOL_VERSION,
                appliance_version: "0.1.0-test".to_string(),
                vm_ip,
            },
        )
        .await;

        while let Some(Ok(request)) = reader.next().await {
            let reply = match request {
                AgentRequest::Up(args) => up_reply(args, &behavior, &journal, vm_ip).await,
                AgentRequest::Down { id, instance } => {
                    // Тестовый гейт (см. AgentJournal::down_gate).
                    let _held = journal.down_gate.lock().await;
                    down_reply(id, instance, &journal)
                }
                AgentRequest::Status { id: None } => {
                    journal
                        .probes
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    AgentReply::Status {
                        bottles: Vec::new(),
                    }
                }
                AgentRequest::Status { .. } => AgentReply::Status {
                    bottles: Vec::new(),
                },
                AgentRequest::Shutdown => AgentReply::Ok,
                AgentRequest::Hello { .. } => AgentReply::Error {
                    code: err::BAD_REQUEST.to_string(),
                    message: "повторный Hello".to_string(),
                },
            };
            send_frame(&mut write_half, &mut codec, reply).await;
        }
    }

    /// Ответ на `Down` по семантике протокола v2: бутылки нет —
    /// `NOT_FOUND`; чужое поколение — `STALE_INSTANCE`, бутылка и relay
    /// живы; своё поколение — разбор relay и `Ok`.
    fn down_reply(id: String, instance: u64, journal: &AgentJournal) -> AgentReply {
        journal.downs.lock().unwrap().push((id.clone(), instance));
        let owned = journal.live.lock().unwrap().get(&id).copied();
        match owned {
            None => AgentReply::Error {
                code: err::NOT_FOUND.to_string(),
                message: format!("бутылка {id} не найдена"),
            },
            Some(live_instance) if live_instance != instance => AgentReply::Error {
                code: err::STALE_INSTANCE.to_string(),
                message: format!(
                    "бутылка {id} принадлежит поколению {live_instance}, запрос — {instance}; \
                     бутылка не тронута"
                ),
            },
            Some(_) => {
                journal.live.lock().unwrap().remove(&id);
                journal.drop_relays(&id, instance);
                AgentReply::Ok
            }
        }
    }

    /// Ответ на `Up` по сценарию; relay поднимаются до ответа `Ready`
    /// (как настоящий агент — спека §7.1, шаг 5).
    async fn up_reply(
        args: UpArgs,
        behavior: &UpBehavior,
        journal: &AgentJournal,
        vm_ip: Ipv4Addr,
    ) -> AgentReply {
        let attempt = {
            let mut ups = journal.ups.lock().unwrap();
            ups.push(args.clone());
            ups.len()
        };
        let ready = || async {
            let mut handles = vec![spawn_socks_relay(vm_ip, args.socks5_port).await];
            for forward in &args.forwards {
                handles.push(spawn_echo_relay(vm_ip, forward.port).await);
            }
            journal.remember_relays(&args.id, args.instance, handles);
            // Бутылка регистрируется до ответа Ready — как в настоящем
            // агенте (реестр Bottles пополняется перед Ready).
            journal
                .live
                .lock()
                .unwrap()
                .insert(args.id.clone(), args.instance);
            AgentReply::Ready {
                id: args.id.clone(),
            }
        };
        match behavior {
            UpBehavior::Ready => ready().await,
            UpBehavior::PortBusyOnce if attempt == 1 => AgentReply::Error {
                code: err::PORT_BUSY.to_string(),
                message: "порт занят внутри аплаенса".to_string(),
            },
            UpBehavior::PortBusyOnce => ready().await,
            UpBehavior::PortBusyAlways => AgentReply::Error {
                code: err::PORT_BUSY.to_string(),
                message: "порт занят внутри аплаенса".to_string(),
            },
            UpBehavior::TunnelFailed(message) => AgentReply::Error {
                code: err::TUNNEL_FAILED.to_string(),
                message: message.clone(),
            },
            UpBehavior::DelayedFirstUp(delay) => {
                if attempt == 1 {
                    tokio::time::sleep(*delay).await;
                }
                ready().await
            }
            UpBehavior::DelayedFirstUpBarrier(delay) => {
                // Строгая очередь `Up`-ов: первый занимает гейт на время
                // «handshake туннеля», следующий ждёт и приходит к
                // барьеру уже после регистрации предыдущего.
                let _gate = journal.up_gate.lock().await;
                if journal.live.lock().unwrap().contains_key(&args.id) {
                    return AgentReply::Error {
                        code: err::ALREADY_UP.to_string(),
                        message: format!("бутылка {} уже поднята", args.id),
                    };
                }
                if attempt == 1 {
                    tokio::time::sleep(*delay).await;
                }
                ready().await
            }
        }
    }

    /// SOCKS5-relay: greeting `\x05\x01\x00` → `\x05\x00`, дальше эхо.
    async fn spawn_socks_relay(vm_ip: Ipv4Addr, port: u16) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind((vm_ip, port))
            .await
            .expect("bind socks relay");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut greeting = [0u8; 3];
                    if sock.read_exact(&mut greeting).await.is_err() {
                        return;
                    }
                    assert_eq!(&greeting, b"\x05\x01\x00", "SOCKS5 greeting клиента");
                    if sock.write_all(b"\x05\x00").await.is_err() {
                        return;
                    }
                    echo_loop(&mut sock).await;
                });
            }
        })
    }

    /// Эхо-relay проброса: возвращает полученные байты.
    async fn spawn_echo_relay(vm_ip: Ipv4Addr, port: u16) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind((vm_ip, port))
            .await
            .expect("bind echo relay");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    echo_loop(&mut sock).await;
                });
            }
        })
    }

    async fn echo_loop(sock: &mut TcpStream) {
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
    }

    /// Кадрует и отправляет ответ фейк-агента.
    async fn send_frame(
        write_half: &mut OwnedWriteHalf,
        codec: &mut LengthJsonCodec<AgentReply>,
        reply: AgentReply,
    ) {
        let mut buf = BytesMut::new();
        codec.encode(reply, &mut buf).expect("кодирование ответа");
        write_half.write_all(&buf).await.expect("отправка ответа");
    }

    /// Менеджер над хранилищем и фейк-агентом.
    fn manager_with(store_root: &std::path::Path, fake: FakeAgent) -> (Arc<BottleManager>, Store) {
        let store = Store::open(store_root).unwrap();
        (
            Arc::new(BottleManager::new(store, Arc::new(fake))),
            Store::open(store_root).unwrap(),
        )
    }

    /// Менеджер над хранилищем и фейк-агентом с инъекциями живучести
    /// (сжатый бэкофф reconnect для таймлайнов health-цикла).
    fn manager_with_timing(
        store_root: &std::path::Path,
        fake: FakeAgent,
        timing: ResilienceTiming,
    ) -> (Arc<BottleManager>, Store) {
        let store = Store::open(store_root).unwrap();
        (
            Arc::new(BottleManager::new(store, Arc::new(fake)).with_resilience_timing(timing)),
            Store::open(store_root).unwrap(),
        )
    }

    /// Читает seed запуска из `generation-seed` корня данных.
    fn read_generation_seed(data_root: &std::path::Path) -> u64 {
        let text =
            std::fs::read_to_string(data_root.join("generation-seed")).expect("файл поколения");
        text.trim().parse().expect("seed — десятичное u64")
    }

    /// Рантайм-состояние бутылки `id` из status (одна должна быть).
    async fn runtime_of(manager: &BottleManager, id: &str) -> RuntimeState {
        manager
            .status()
            .await
            .into_iter()
            .find(|b| b.id == id)
            .unwrap_or_else(|| panic!("нет бутылки {id} в status"))
    }

    /// Крутится до состояния, удовлетворяющего `want` (реальный дедлайн
    /// 5 с, шаг 5 мс) — для таймлайнов фонового health-цикла. Отсутствие
    /// бутылки в status — не ошибка: между teardown и пере-подъёмом
    /// восстановления записи нет (транзитное окно).
    async fn wait_state(
        manager: &BottleManager,
        id: &str,
        want: fn(&BottleState) -> bool,
        what: &str,
    ) -> RuntimeState {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(state) = manager.status().await.into_iter().find(|b| b.id == id) {
                if want(&state.state) {
                    return state;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "не дождались: {what}; текущее состояние: {:?}",
                    state.state
                );
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "не дождались: {what}; бутылки {id} нет в status"
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn is_up(state: &BottleState) -> bool {
        matches!(state, BottleState::Up)
    }

    fn is_degraded_by_agent(state: &BottleState) -> bool {
        matches!(
            state,
            BottleState::Degraded { reason } if reason.contains("агент аплаенса не отвечает")
        )
    }

    #[tokio::test]
    async fn start_answers_ready_socks5_works_and_status_is_up() {
        let (_tmp, store) = store_with(&env_profile("office-a", "ignored-by-save.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _dispatch_store) = manager_with(store.data_root(), fake.clone());

        let reply = manager.start("office-a", None).await.expect("start ок");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            }
        );

        // Статус: одна бутылка, up, порты назначены из динамического диапазона.
        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 1, "{bottles:?}");
        let rt = &bottles[0];
        assert_eq!(rt.id, "office-a");
        assert!(matches!(rt.state, BottleState::Up), "{:?}", rt.state);
        let socks5 = rt.socks5.expect("порт socks5 назначен");
        assert!(
            (PORT_DYNAMIC_MIN..=PORT_DYNAMIC_MAX).contains(&socks5),
            "socks5 {socks5} из динамического диапазона"
        );
        assert_eq!(rt.forwards.len(), 1);
        assert_eq!(rt.forwards[0].target, "10.20.30.15:3389");
        assert!(
            (PORT_DYNAMIC_MIN..=PORT_DYNAMIC_MAX).contains(&rt.forwards[0].port),
            "порт проброса из динамического диапазона"
        );

        // SOCKS5 через слушатель демона: greeting проходит, данные до эха.
        let mut sock = TcpStream::connect(("127.0.0.1", socks5))
            .await
            .expect("подключение к socks5 демона");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00", "эхо за туннелем ответило no-auth");
        sock.write_all(b"ping").await.expect("данные");
        let mut echoed = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut echoed))
            .await
            .expect("эхо в пределах тайм-аута")
            .expect("чтение эха");
        assert_eq!(&echoed, b"ping");

        // Агент получил ровно один Up с содержимым конфига, конкретными
        // портами (socks5_port=0 не отправляется — см. T11) и
        // instance-поколением старта: старшие биты — seed запуска из
        // generation-seed, младшие — счётчик (первый start — 0).
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].id, "office-a");
        assert_eq!(ups[0].protocol, TunnelProtocol::WireGuard);
        assert_eq!(ups[0].config, "wg-office-a", "конфиг передан содержимым");
        assert_eq!(ups[0].dns, Some("10.20.30.1".parse().unwrap()));
        assert_ne!(ups[0].socks5_port, 0, "нулевые порты не отправляются");
        assert_ne!(ups[0].forwards[0].port, 0, "нулевые порты не отправляются");
        let seed = read_generation_seed(store.data_root());
        assert_eq!(
            ups[0].instance,
            seed << 20,
            "Up несёт персистентное instance-поколение"
        );
    }

    /// OpenVPN-профиль с голым `auth-user-pass`: start без кредов —
    /// `NeedCredentials` ДО обращения к аплаенсу (ни одного Up, бутылка
    /// не помечается failed); start с кредами — `Started`, агент получил
    /// их в `Up`. Креды не хранятся: каждый запуск спрашивает заново.
    #[tokio::test]
    async fn openvpn_auth_user_pass_requires_credentials_per_start() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("yorus.conf");
        std::fs::write(&src, "client\nauth-user-pass\nremote vpn.example 443\n").unwrap();
        let mut profile = env_profile("yorus-new", src.to_str().unwrap());
        profile.vpn = VpnConfig::Env {
            protocol: TunnelProtocol::OpenVpn,
            config: src.to_string_lossy().into_owned(),
        };
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store.save(&profile).unwrap();

        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        // Без кредов: запрос кредов, аплаенс не дёргается.
        let reply = manager
            .start("yorus-new", None)
            .await
            .expect("ответ, не сбой");
        assert_eq!(
            reply,
            CliReply::NeedCredentials {
                id: "yorus-new".into()
            }
        );
        assert!(
            fake.journal.ups().is_empty(),
            "предпроверка кредов — до connect/Up"
        );
        assert!(
            manager.status().await.is_empty(),
            "бутылка не помечается failed/starting"
        );

        // С кредами: обычный старт, Up несёт логин/пароль.
        let reply = manager
            .start(
                "yorus-new",
                Some(VpnCredentials {
                    username: "user@corp".to_string(),
                    password: "s3cret".to_string(),
                }),
            )
            .await
            .expect("start с кредами ок");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "yorus-new".into()
            }
        );
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].username.as_deref(), Some("user@corp"));
        assert_eq!(ups[0].password.as_deref(), Some("s3cret"));
    }

    /// Конфиг OpenVPN с `auth-user-pass <файл>` (не голый) кредов не
    /// требует: start идёт сразу.
    #[tokio::test]
    async fn openvpn_auth_file_form_starts_without_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("ovpn.conf");
        std::fs::write(&src, "client\nauth-user-pass /etc/secret.txt\n").unwrap();
        let mut profile = env_profile("ovpn-file", src.to_str().unwrap());
        profile.vpn = VpnConfig::Env {
            protocol: TunnelProtocol::OpenVpn,
            config: src.to_string_lossy().into_owned(),
        };
        let store = Store::open(&tmp.path().join("store")).unwrap();
        store.save(&profile).unwrap();

        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let reply = manager.start("ovpn-file", None).await.expect("start ок");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "ovpn-file".into()
            }
        );
        assert_eq!(fake.journal.ups().len(), 1);
        assert_eq!(fake.journal.ups()[0].username, None);
    }

    #[tokio::test]
    async fn second_start_on_up_is_noop_with_single_bottle() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        manager.start("office-a", None).await.expect("первый start");
        let reply = manager
            .start("office-a", None)
            .await
            .expect("повторный start без ошибки");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            },
            "повторный start — no-op с ответом Started"
        );

        assert_eq!(manager.status().await.len(), 1, "бутылка по-прежнему одна");
        assert_eq!(
            fake.journal.ups().len(),
            1,
            "второй Up не отправляется (RF#3)"
        );
    }

    #[tokio::test]
    async fn port_busy_reselects_ports_and_retries_up_once() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::PortBusyOnce).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let reply = manager
            .start("office-a", None)
            .await
            .expect("start после ретрая");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            }
        );

        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 2, "ровно один повтор Up (RF#5)");
        assert_ne!(
            ups[1].socks5_port, ups[0].socks5_port,
            "повтор идёт с другими портами"
        );
        assert_ne!(ups[1].forwards[0].port, ups[0].forwards[0].port);
        assert_eq!(
            ups[1].instance, ups[0].instance,
            "повтор Up — то же поколение старта"
        );

        // Итоговое состояние показывает порты второй попытки.
        let bottles = manager.status().await;
        assert!(matches!(bottles[0].state, BottleState::Up));
        assert_eq!(bottles[0].socks5, Some(ups[1].socks5_port));
        assert_eq!(bottles[0].forwards[0].port, ups[1].forwards[0].port);
    }

    #[tokio::test]
    async fn port_busy_retry_keeps_fixed_ports_and_replaces_dynamic() {
        let fixed_socks5 = free_host_port();
        let mut profile = env_profile("fixed-a", "wg.conf");
        // Фиксированный SOCKS5, проброс остаётся динамическим.
        profile.expose.socks5 = fixed_socks5;

        let (_tmp, store) = store_with(&profile);
        let fake = FakeAgent::spawn(UpBehavior::PortBusyOnce).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        // Первый Up — PORT_BUSY; повтор сохраняет фиксированный порт
        // (без повторного bind) и перевыбирает динамический.
        let reply = manager
            .start("fixed-a", None)
            .await
            .expect("start после ретрая");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "fixed-a".into()
            }
        );

        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 2, "ровно один повтор Up");
        assert_eq!(
            ups[1].socks5_port, ups[0].socks5_port,
            "фиксированный порт сохранён между попытками"
        );
        assert_eq!(ups[1].socks5_port, fixed_socks5);
        assert_ne!(
            ups[1].forwards[0].port, ups[0].forwards[0].port,
            "динамический порт перевыбран"
        );

        let bottles = manager.status().await;
        assert!(matches!(bottles[0].state, BottleState::Up));
        assert_eq!(bottles[0].socks5, Some(fixed_socks5));
        assert_eq!(bottles[0].forwards[0].port, ups[1].forwards[0].port);

        // Фиксированный SOCKS5 действительно обслуживается через relay агента.
        let mut sock = TcpStream::connect(("127.0.0.1", fixed_socks5))
            .await
            .expect("подключение к фиксированному socks5");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00", "эхо за туннелем ответило no-auth");
    }

    #[tokio::test]
    async fn port_busy_on_fixed_port_reports_agent_conflict_after_retry() {
        let fixed_socks5 = free_host_port();
        let mut profile = env_profile("fixed-b", "wg.conf");
        profile.expose.socks5 = fixed_socks5;

        let (_tmp, store) = store_with(&profile);
        let fake = FakeAgent::spawn(UpBehavior::PortBusyAlways).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let reply = manager
            .start("fixed-b", None)
            .await
            .expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        // Причиной назван конфликт на стороне агента (и порт профиля),
        // а не локальный EADDRINUSE от самого ретрая.
        assert!(message.contains("PORT_BUSY"), "код агента: {message}");
        assert!(
            message.contains(&fixed_socks5.to_string()),
            "назван фиксированный порт: {message}"
        );
        assert!(
            message.contains("на стороне агента"),
            "конфликт отнесён стороне агента: {message}"
        );
        assert!(
            !message.contains("занят (SOCKS5)"),
            "не путать с локальной занятостью порта: {message}"
        );

        // Повтор был; бутылка failed, фиксированный порт освобождён.
        assert_eq!(fake.journal.ups().len(), 2, "повтор Up выполнялся");
        let bottles = manager.status().await;
        assert!(
            matches!(&bottles[0].state, BottleState::Failed { reason } if reason.contains("PORT_BUSY")),
            "{:?}",
            bottles[0].state
        );
        drop(
            std::net::TcpListener::bind(("127.0.0.1", fixed_socks5))
                .expect("фиксированный порт свободен"),
        );
    }

    #[tokio::test]
    async fn tunnel_failure_marks_bottle_failed_and_frees_ports() {
        const REASON: &str = "туннель не поднялся: handshake timeout";
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::TunnelFailed(REASON.to_string())).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let reply = manager
            .start("office-a", None)
            .await
            .expect("start отвечает ошибкой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(message.contains(REASON), "в сообщении причина: {message}");

        // Бутылка failed с причиной; порты освобождены.
        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 1);
        assert!(
            matches!(&bottles[0].state, BottleState::Failed { reason } if reason.contains(REASON)),
            "{:?}",
            bottles[0].state
        );
        assert_eq!(bottles[0].socks5, None);

        let first = fake.journal.ups();
        assert_eq!(first.len(), 1);
        let socks5 = first[0].socks5_port;
        let forward = first[0].forwards[0].port;
        drop(std::net::TcpListener::bind(("127.0.0.1", socks5)).expect("socks5-порт свободен"));
        drop(std::net::TcpListener::bind(("127.0.0.1", forward)).expect("порт проброса свободен"));

        // Бутылки в аплаенсе нет — Down не отправляется.
        assert!(
            fake.journal.downs().is_empty(),
            "Down не нужен после ошибки Up"
        );
    }

    #[tokio::test]
    async fn stop_sends_down_closes_listeners_and_clears_status() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        // Stop незапущенной бутылки — no-op с предупреждением (§5.2).
        let reply = manager.stop("office-a").await.expect("stop без паники");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: Some("не была запущена".to_string())
            }
        );

        manager.start("office-a", None).await.expect("start");
        let socks5 = manager.status().await[0].socks5.expect("порт назначен");

        let reply = manager.stop("office-a").await.expect("stop ок");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: None
            }
        );

        // Агенту ушёл Down с поколением поднятой бутылки; слушатели
        // закрыты; из status бутылка исчезла.
        let instance = fake.journal.ups()[0].instance;
        assert_eq!(
            fake.journal.downs(),
            vec![("office-a".to_string(), instance)],
            "Down несёт instance-поколение"
        );
        assert!(
            TcpStream::connect(("127.0.0.1", socks5)).await.is_err(),
            "порт закрыт после stop"
        );
        assert!(manager.status().await.is_empty());
    }

    #[tokio::test]
    async fn stale_instance_down_spares_live_bottle_and_is_tolerated() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        manager.start("office-a", None).await.expect("start"); // поколение 0
        let socks5 = manager.status().await[0].socks5.expect("порт назначен");

        // Down чужого поколения: агент отвечает STALE_INSTANCE — для
        // демона это не ошибка (своя бутылка уже заменена живой чужой).
        manager
            .send_down("office-a", 999)
            .await
            .expect("STALE_INSTANCE на Down tolerated");

        // Бутылка в аплаенсе жива: relay агента по-прежнему обслуживает
        // SOCKS5 (фейк-агент не разобрал её по чужому Down).
        let mut sock = TcpStream::connect(("127.0.0.1", socks5))
            .await
            .expect("relay жив после STALE_INSTANCE");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00", "эхо за туннелем ответило");

        // Рантайм демона не тронут; корректный stop со своим поколением
        // разбирает бутылку как обычно.
        assert_eq!(manager.status().await.len(), 1);
        let reply = manager.stop("office-a").await.expect("stop ок");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: None
            }
        );
        assert_eq!(
            fake.journal.downs(),
            vec![
                ("office-a".to_string(), 999),
                ("office-a".to_string(), fake.journal.ups()[0].instance),
            ],
            "чужой Down затем свой Down"
        );
    }

    #[tokio::test]
    async fn restart_during_start_leaves_single_live_bottle() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        // Первый Up застревает в агенте на 300 мс — окно для stop+restart.
        let fake = FakeAgent::spawn(UpBehavior::DelayedFirstUp(Duration::from_millis(300))).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let first = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.start("office-a", None).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await; // start1 уже в фазе Up
        manager
            .stop("office-a")
            .await
            .expect("stop во время старта");
        let reply = manager.start("office-a", None).await.expect("restart");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            }
        );

        // Проигравший старт разбирает свои ресурсы и отвечает ошибкой.
        let reply = first.await.expect("join start1").expect("без паники");
        assert!(
            matches!(reply, CliReply::Error { .. }),
            "проигравший старт отвечает ошибкой: {reply:?}"
        );

        // Ровно одна живая бутылка; её SOCKS5 реально обслуживается.
        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 1, "{bottles:?}");
        assert!(matches!(bottles[0].state, BottleState::Up));
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 2, "Up дважды: проигравший и победитель");
        assert_ne!(
            ups[0].instance, ups[1].instance,
            "рестарт — новое поколение в Up"
        );
        let socks5 = bottles[0].socks5.expect("порт назначен");
        assert_eq!(socks5, ups[1].socks5_port, "жив порт победителя");
        let mut sock = TcpStream::connect(("127.0.0.1", socks5))
            .await
            .expect("socks5 победителя жив");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00");

        // Порты проигравшего освобождены — ни слушателей, ни задач-сирот.
        drop(
            std::net::TcpListener::bind(("127.0.0.1", ups[0].socks5_port))
                .expect("socks5 проигравшего свободен"),
        );
        drop(
            std::net::TcpListener::bind(("127.0.0.1", ups[0].forwards[0].port))
                .expect("проброс проигравшего свободен"),
        );

        // Down дважды, оба с поколением проигравшего старта: от stop
        // (бутылки в аплаенсе ещё не было — NOT_FOUND) и от teardown
        // проигравшего (Finding 2: teardown ВСЕГДА посылает Down своего
        // поколения). Бутылке-победителю они не вредит: агент опускает
        // только совпавшее поколение, релеи победителя не тронуты.
        let loser_instance = fake.journal.ups()[0].instance;
        assert_eq!(
            fake.journal.downs(),
            vec![
                ("office-a".to_string(), loser_instance),
                ("office-a".to_string(), loser_instance),
            ],
            "Down от stop и Down от teardown проигравшего, оба его поколения"
        );
    }

    /// Finding 2 (ревью): гонка stop→restart оставляет «бутылку-призрак»
    /// проигравшего старта в аплаенсе. Последовательный агент с барьером
    /// id: первый `Up` «поднимает туннель» дольше, чем идут stop+restart,
    /// и регистрирует свою бутылку к моменту, когда `Up` победителя
    /// дойдёт до барьера → победитель получает `ALREADY_UP` и падает.
    /// Teardown проигравшего обязан отправить `Down` СВОЕГО поколения и
    /// убрать призрака — иначе id заблокирован навсегда (каждый следующий
    /// старт — снова `ALREADY_UP`).
    #[tokio::test]
    async fn restart_race_loser_down_clears_ghost_bottle() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        // Первый Up держит гейт 300 мс («долгий handshake туннеля»):
        // Up победителя встаёт в очередь и приходит к барьеру ПОСЛЕ
        // регистрации проигравшего → ALREADY_UP.
        let fake = FakeAgent::spawn(UpBehavior::DelayedFirstUpBarrier(Duration::from_millis(
            300,
        )))
        .await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let first = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.start("office-a", None).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await; // start1 в фазе Up
        manager
            .stop("office-a")
            .await
            .expect("stop во время старта (Down ещё NOT_FOUND)");
        let reply = manager
            .start("office-a", None)
            .await
            .expect("restart отвечает");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error (ALREADY_UP), получили {reply:?}")
        };
        assert!(
            message.contains("ALREADY_UP"),
            "победитель упёрся в призрака: {message}"
        );

        // Проигравший старт получил свой Ready, но запись в карте уже
        // чужая: teardown разбирает ресурсы и отправляет Down СВОЕГО
        // поколения — призрак снят с аплаенса.
        let reply = first.await.expect("join start1").expect("без паники");
        assert!(
            matches!(reply, CliReply::Error { .. }),
            "проигравший старт отвечает ошибкой: {reply:?}"
        );
        let loser_instance = fake.journal.ups()[0].instance;
        assert!(
            fake.journal
                .downs()
                .contains(&("office-a".to_string(), loser_instance)),
            "Down поколения проигравшего убрал призрака: {:?}",
            fake.journal.downs()
        );

        // Призрака нет — следующий старт проходит (никакого вечного
        // цикла ALREADY_UP) и бутылка реально работает.
        let reply = manager
            .start("office-a", None)
            .await
            .expect("start после уборки призрака");
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            }
        );
        let bottles = manager.status().await;
        assert_eq!(bottles.len(), 1, "{bottles:?}");
        assert!(matches!(bottles[0].state, BottleState::Up));
        let socks5 = bottles[0].socks5.expect("порт назначен");
        let mut sock = TcpStream::connect(("127.0.0.1", socks5))
            .await
            .expect("socks5 жив после уборки призрака");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00");
    }

    #[tokio::test]
    async fn stop_during_start_tears_down_and_sends_down() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::DelayedFirstUp(Duration::from_millis(300))).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let first = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.start("office-a", None).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let reply = manager
            .stop("office-a")
            .await
            .expect("stop во время старта");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: None
            }
        );

        let reply = first.await.expect("join start1").expect("без паники");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(message, "бутылка остановлена во время запуска");

        // Up успел дойти и создать бутылку в аплаенсе — teardown отправил
        // Down за него (второй Down после Down самого stop); оба несут
        // поколение проигравшего старта.
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 1);
        assert_eq!(
            fake.journal.downs(),
            vec![
                ("office-a".to_string(), ups[0].instance),
                ("office-a".to_string(), ups[0].instance),
            ],
            "Down от stop и Down от teardown"
        );

        // Порты проигравшего старта освобождены; бутылки в карте нет.
        drop(
            std::net::TcpListener::bind(("127.0.0.1", ups[0].socks5_port))
                .expect("socks5-порт свободен"),
        );
        drop(
            std::net::TcpListener::bind(("127.0.0.1", ups[0].forwards[0].port))
                .expect("порт проброса свободен"),
        );
        assert!(manager.status().await.is_empty());
    }

    /// Host-старт без службы маршрутов — текст установки T7 (спека §2:
    /// без UAC-fallback). Проверка идёт раньше любых эффектов.
    #[tokio::test]
    async fn host_profile_start_without_svc_answers_installation_message() {
        let (_tmp, store) = store_with(&host_profile("corp-vpn"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let reply = manager
            .start("corp-vpn", None)
            .await
            .expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(
            message,
            crate::svc_client::SVC_MISSING_MESSAGE,
            "текст установки службы T7"
        );
        assert!(
            fake.journal.ups().is_empty(),
            "к агенту host-профиль не доходит"
        );
        // Состояния нет: бутылка не поднималась (предусловие).
        assert!(manager.status().await.is_empty());
    }

    #[tokio::test]
    async fn missing_config_file_fails_the_bottle_with_clear_reason() {
        let (tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());
        std::fs::remove_file(tmp.path().join("store/bottles/office-a.conf")).unwrap();

        let reply = manager
            .start("office-a", None)
            .await
            .expect("ответ, не сбой");
        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("office-a.conf"),
            "в сообщении имя конфига: {message}"
        );

        let bottles = manager.status().await;
        assert!(matches!(&bottles[0].state, BottleState::Failed { .. }));
        assert!(
            fake.journal.ups().is_empty(),
            "Up без конфига не отправляется"
        );
    }

    /// Удаление активной бутылки через диспетчер — отказ с подсказкой
    /// остановить; файлы профиля остаются на месте.
    #[tokio::test]
    async fn dispatch_delete_of_active_bottle_is_refused() {
        let (tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, store) = manager_with(store.data_root(), fake);
        manager
            .start("office-a", None)
            .await
            .expect("бутылка поднята");

        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Delete {
                id: "office-a".to_string(),
            },
        )
        .await;

        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("остановите"),
            "подсказка остановки: {message}"
        );
        assert!(
            tmp.path().join("store/bottles/office-a.toml").exists(),
            "профиль не тронут"
        );
    }

    /// Провалившаяся бутылка (Failed) удаляется без остановки:
    /// Failed-запись — не активная сессия (namespace разобран при
    /// неудаче старта), «остановите бутылку» для неё — дед-энд
    /// (finding 1 финального ревью: stop на Failed — no-op).
    #[tokio::test]
    async fn dispatch_delete_of_failed_bottle_succeeds() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::TunnelFailed(
            "туннель не поднялся: handshake timeout".to_string(),
        ))
        .await;
        let (manager, store) = manager_with(store.data_root(), fake);
        let reply = manager
            .start("office-a", None)
            .await
            .expect("ответ, не сбой");
        assert!(matches!(reply, CliReply::Error { .. }), "start провалился");
        assert!(manager
            .status()
            .await
            .iter()
            .any(|state| matches!(state.state, BottleState::Failed { .. })));

        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Delete {
                id: "office-a".to_string(),
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Deleted {
                id: "office-a".into(),
                warning: None
            },
            "Failed-бутылка удаляема без stop"
        );
    }

    /// Upgrade при живой бутылке — отказ (Review Focus M3 #2): сносить
    /// дистрибуцию под работающими бутылками нельзя.
    #[tokio::test]
    async fn dispatch_upgrade_with_live_bottle_is_refused() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, store) = manager_with(store.data_root(), fake);
        manager
            .start("office-a", None)
            .await
            .expect("бутылка поднята");

        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Upgrade,
        )
        .await;

        let CliReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(
            message.contains("остановите"),
            "требование остановить бутылки: {message}"
        );
    }

    /// Повторный stop уже остановленной — no-op с предупреждением,
    /// не ошибка (спека §5.2).
    #[tokio::test]
    async fn dispatch_stop_of_stopped_bottle_is_noop_with_warning() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, store) = manager_with(store.data_root(), fake);
        manager
            .start("office-a", None)
            .await
            .expect("бутылка поднята");
        let first = manager.stop("office-a").await.expect("первый stop");
        assert_eq!(
            first,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: None
            }
        );

        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Stop {
                id: "office-a".to_string(),
            },
        )
        .await;

        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: Some("не была запущена".to_string())
            }
        );
    }

    #[tokio::test]
    async fn dispatch_wires_start_stop_status_and_show_runtime_to_manager() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, store) = manager_with(store.data_root(), fake);

        // Start через диспетчер.
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Start {
                id: "office-a".to_string(),
                credentials: None,
            },
        )
        .await;
        assert_eq!(
            reply,
            CliReply::Started {
                id: "office-a".into()
            }
        );

        // Show видит рантайм поднятой бутылки.
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Show {
                id: "office-a".to_string(),
            },
        )
        .await;
        let CliReply::Show { profile, runtime } = reply else {
            panic!("ожидали Show, получили {reply:?}");
        };
        assert!(profile.is_some());
        let runtime = runtime.expect("рантайм поднятой бутылки");
        assert_eq!(runtime.id, "office-a");
        assert!(matches!(runtime.state, BottleState::Up));

        // Status показывает бутылку.
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Status,
        )
        .await;
        assert_eq!(
            reply,
            CliReply::Status {
                bottles: vec![runtime.clone()]
            }
        );

        // Stop и очистка статуса.
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Stop {
                id: "office-a".to_string(),
            },
        )
        .await;
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: None
            }
        );
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            None,
            None,
            bottle_core::ipc::CliRequest::Status,
        )
        .await;
        assert_eq!(reply, CliReply::Status { bottles: vec![] });
    }

    // ------------------------------------------------------------------
    // Живучесть T11 (спека §9): персистентные поколения, health-цикл,
    // смерти relay-задач.
    // ------------------------------------------------------------------

    /// Seed очередного запуска: без файла — текущее время; при коллизии
    /// содержимого (тот же ms, часы ушли назад) — увеличение.
    #[test]
    fn next_seed_advances_on_collision() {
        const NOW: u64 = 1_700_000_000_123;
        assert_eq!(next_seed(None, NOW), NOW, "первое создание — now");
        assert_eq!(
            next_seed(Some(NOW), NOW),
            NOW + 1,
            "коллизия содержимого — увеличить"
        );
        assert_eq!(
            next_seed(Some(NOW + 7), NOW),
            NOW + 8,
            "часы ушли назад — увеличить за файлом"
        );
        assert_eq!(
            next_seed(Some(NOW - 5), NOW),
            NOW,
            "время ушло вперёд — просто now"
        );
    }

    /// (д) generation-seed: файл создаётся при первом менеджере, повторный
    /// «запуск демона» над тем же каталогом читает/продвигает его;
    /// instance-диапазоны двух запусков не пересекаются (разные seed).
    #[tokio::test]
    async fn generation_seed_persists_and_boot_instances_are_disjoint() {
        let (tmp, store) = store_with(&env_profile("office-a", "wg.conf"));

        // Первый запуск: файл создан, instance = seed << 20 | 0.
        let fake1 = FakeAgent::spawn(UpBehavior::Ready).await;
        let (boot1, _s1) = manager_with(store.data_root(), fake1.clone());
        boot1.start("office-a", None).await.expect("start boot1");
        let seed1 = read_generation_seed(store.data_root());
        let instance1 = fake1.journal.ups()[0].instance;
        assert_eq!(instance1, seed1 << 20, "младшие 20 бит — счётчик с нуля");
        drop(boot1);

        // Второй запуск над тем же каталогом: seed прочитан и продвинут
        // (даже если оба менеджера созданы в одну миллисекунду).
        let fake2 = FakeAgent::spawn(UpBehavior::Ready).await;
        let (boot2, _s2) = manager_with(store.data_root(), fake2.clone());
        let seed2 = read_generation_seed(store.data_root());
        assert!(
            seed2 > seed1,
            "seed второго запуска больше: {seed2} vs {seed1}"
        );
        boot2.start("office-a", None).await.expect("start boot2");
        let instance2 = fake2.journal.ups()[0].instance;
        assert_eq!(instance2, seed2 << 20);
        assert_ne!(
            instance1 >> 20,
            instance2 >> 20,
            "instance-диапазоны запусков не пересекаются"
        );
        assert!(instance2 > instance1);

        // Файл на месте и содержит десятичное u64 (формат переживает
        // перезапуск демона).
        let _ = tmp; // держим каталог живым
        let text = std::fs::read_to_string(store.data_root().join("generation-seed")).unwrap();
        assert_eq!(text.trim().parse::<u64>().unwrap(), seed2);
    }

    /// instance монотонно растёт в пределах запуска (каждый start — новый
    /// номер, общий префикс seed).
    #[tokio::test]
    async fn instances_rise_monotonically_within_boot() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _s) = manager_with(store.data_root(), fake.clone());

        manager.start("office-a", None).await.expect("start 1");
        manager.stop("office-a").await.expect("stop 1");
        manager.start("office-a", None).await.expect("start 2");
        manager.stop("office-a").await.expect("stop 2");
        manager.start("office-a", None).await.expect("start 3");

        let instances: Vec<u64> = fake.journal.ups().iter().map(|up| up.instance).collect();
        assert_eq!(instances.len(), 3);
        assert!(instances[0] < instances[1] && instances[1] < instances[2]);
        let seed = read_generation_seed(store.data_root());
        for instance in instances {
            assert_eq!(instance >> 20, seed, "префикс запуска общий");
        }
    }

    /// Пины продакшн-таймингов живучести: период 10 с, бэкофф 5/15/60/60…
    #[test]
    fn health_period_and_backoff_pins() {
        assert_eq!(HEALTH_CHECK_PERIOD, Duration::from_secs(10));
        let timing = ResilienceTiming::default();
        assert_eq!(timing.backoff(0), Duration::from_secs(5));
        assert_eq!(timing.backoff(1), Duration::from_secs(15));
        assert_eq!(timing.backoff(2), Duration::from_secs(60));
        assert_eq!(
            timing.backoff(700),
            Duration::from_secs(60),
            "дальше — 60 с"
        );
    }

    /// (а-env, б) Таймлайн живучести: агент умер → env-бутылка `Degraded`
    /// (метка агента, порты за бутылкой); агент воскрес → бутылка снова
    /// `up`: повторный Up с ДРУГИМ instance-поколением, Down старого
    /// поколения гасил призрака, свежий socks5-порт слушает и negotiate
    /// проходит, старые слушатели закрыты.
    #[tokio::test]
    async fn agent_death_degrades_env_bottle_and_recovery_restores_it() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let no_backoff = ResilienceTiming {
            reconnect_backoff: vec![Duration::from_millis(0)],
        };
        let (manager, _s) = manager_with_timing(store.data_root(), fake.clone(), no_backoff);
        let shutdown = tokio_util::sync::CancellationToken::new();
        manager.spawn_health_loop(shutdown.clone(), Duration::from_millis(25));

        manager.start("office-a", None).await.expect("start");
        let first = wait_state(&manager, "office-a", is_up, "первичный подъём").await;
        let socks5_first = first.socks5.expect("порт назначен");
        let instance_first = fake.journal.ups()[0].instance;

        // Агент умер (wsl --shutdown): ≤ пары периодов — Degraded.
        fake.kill();
        let degraded = wait_state(
            &manager,
            "office-a",
            is_degraded_by_agent,
            "деградация после смерти агента",
        )
        .await;
        assert!(degraded.socks5.is_some(), "порты остаются за бутылкой");

        // Агент воскрес (рестарт VM с чистым реестром): бутылка снова up.
        fake.resurrect();
        let restored = wait_state(&manager, "office-a", is_up, "восстановление").await;

        // Повторный Up — новое instance-поколение того же запуска
        // (старшие биты — seed, младшие — следующий счётчик).
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 2, "ровно один повторный Up: {:?}", ups);
        assert_ne!(
            ups[1].instance, instance_first,
            "поколение восстановления новое"
        );
        assert_eq!(
            ups[1].instance >> 20,
            instance_first >> 20,
            "тот же запуск демона — тот же seed"
        );
        // Призрак убран: перед повторным Up уходил Down старого поколения.
        assert!(
            fake.journal
                .downs()
                .contains(&("office-a".to_string(), instance_first)),
            "Down старого поколения: {:?}",
            fake.journal.downs()
        );

        // Новый socks5-порт слушает и обслуживает negotiate (новые порты
        // допустимы — ассерт «какой-то порт слушает и negotiate проходит»).
        let socks5_restored = restored.socks5.expect("порт восстановления");
        let mut sock = TcpStream::connect(("127.0.0.1", socks5_restored))
            .await
            .expect("подключение к восстановленному socks5");
        sock.write_all(b"\x05\x01\x00").await.expect("greeting");
        let mut method = [0u8; 2];
        sock.read_exact(&mut method).await.expect("выбор метода");
        assert_eq!(&method, b"\x05\x00", "воскресший релей ответил no-auth");

        // Старые слушатели закрыты (если порт не переиспользован).
        if socks5_restored != socks5_first {
            assert!(
                TcpStream::connect(("127.0.0.1", socks5_first))
                    .await
                    .is_err(),
                "старый порт закрыт после восстановления"
            );
        }

        // Цикл стабилен: агент жив, лишних Up нет.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fake.journal.ups().len(),
            2,
            "повторных Up при живом агенте нет"
        );
        shutdown.cancel();
    }

    /// Деградация не трогает host-часть и не требует бэкоффа при живом
    /// агенте: бутылка остаётся up, Up не дублируются.
    #[tokio::test]
    async fn health_pass_with_alive_agent_keeps_bottle_up() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _s) = manager_with(store.data_root(), fake.clone());

        manager.start("office-a", None).await.expect("start");
        for _ in 0..3 {
            manager.health_pass().await;
        }
        assert!(matches!(
            runtime_of(&manager, "office-a").await.state,
            BottleState::Up
        ));
        assert_eq!(fake.journal.ups().len(), 1, "лишних Up нет");
    }

    /// (в-env) Смерть relay-задачи (не по остановке): бутылка получает
    /// `Degraded{причина}` — не `failed`, слушатели живут.
    /// Окно восстановления vs пользовательский stop (рулинг
    /// M3-бэклога): health уже снял бутылку (свой stop), start ещё не
    /// начался. Пользовательский stop в этом окне — команда «не
    /// поднимай обратно»: восстановление обязано пропустить start.
    ///
    /// Хореография: гейт Down задерживает ответ агенту → restore паркует
    /// в send_down ПОСЛЕ вынимания записи из runtime → no-op stop
    /// пользователя попадает точно между stop и start восстановления.
    #[tokio::test]
    async fn user_stop_during_recovery_window_prevents_repost() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());
        manager.start("office-a", None).await.expect("поднята");

        // Смерть агента → деградация; воскрешение с зажатым Down-гейтом.
        fake.kill();
        manager.health_pass().await;
        let gate = fake.journal.down_gate.clone();
        let _held = gate.lock().await;
        fake.resurrect();

        // Восстановление в фоне: паркуется в Down (гейт).
        let restoring = tokio::spawn({
            let manager = manager.clone();
            async move { manager.restore_after_recovery().await }
        });
        // Ждём вынимания бутылки из runtime (stop восстановления
        // начался; entry ушёл из карты до send_down).
        let mut removed = false;
        for _ in 0..500 {
            if manager.status().await.is_empty() {
                removed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(removed, "восстановление начало свой stop");

        // Пользовательский stop в окне — no-op с предупреждением.
        let reply = manager.stop("office-a").await.expect("ответ, не сбой");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".into(),
                warning: Some("не была запущена".to_string())
            }
        );

        // Отпускаем Down: восстановление обязано увидеть флаг и НЕ
        // поднимать бутылку обратно.
        drop(_held);
        restoring.await.expect("restore завершился");

        assert!(
            manager.status().await.is_empty(),
            "бутылка не поднята обратно против воли пользователя"
        );
        assert_eq!(
            fake.journal.ups().len(),
            1,
            "второго Up не было: {:?}",
            fake.journal.ups().len()
        );
    }

    /// Пин T11-концерна (M3-бэклог): без активных env-бутылок
    /// health-проход не будит WSL — alive-пробы не уходят; с живой
    /// бутылкой агент опрашивается.
    #[tokio::test]
    async fn health_pass_skips_agent_probe_without_active_env_bottles() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        // Ни одной бутылки: три прохода — ноль проб агента.
        for _ in 0..3 {
            manager.health_pass().await;
        }
        assert_eq!(
            fake.journal.probes(),
            0,
            "восстанавливать некого — VM пусть спит (никаких wsl.exe-проб)"
        );

        // Живая бутылка: проба обязательна.
        manager.start("office-a", None).await.expect("поднята");
        fake.journal.reset_probes();
        manager.health_pass().await;
        assert!(
            fake.journal.probes() >= 1,
            "живая бутылка — агент опрашивается"
        );
    }

    #[tokio::test]
    async fn relay_task_death_degrades_bottle_with_reason() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let store_root = store.data_root().to_path_buf();
        let manager = Arc::new(
            BottleManager::new(Store::open(&store_root).unwrap(), Arc::new(fake.clone()))
                .with_relay_serve(Arc::new(|_listener, _target| {
                    // Фейк-релей: акцепт-цикл падает сразу (модель смерти
                    // слушателя); исходный слушатель жив в рантайме.
                    Box::pin(async {
                        Err(std::io::Error::other("фейк: акцепт-цикл релея умер"))
                    })
                })),
        );

        manager.start("office-a", None).await.expect("start");

        // Задача умерла сразу после старта; health-цикл (сжатый период)
        // подбирает доклад и помечает бутылку деградированной.
        let shutdown = tokio_util::sync::CancellationToken::new();
        manager.spawn_health_loop(shutdown.clone(), Duration::from_millis(25));
        let degraded = wait_state(
            &manager,
            "office-a",
            |state| matches!(state, BottleState::Degraded { .. }),
            "деградация после смерти релея",
        )
        .await;
        shutdown.cancel();
        assert!(
            matches!(&degraded.state, BottleState::Degraded { reason } if reason.contains("релей")),
            "причина от релея: {:?}",
            degraded.state
        );
        assert!(degraded.socks5.is_some(), "порт остаётся за бутылкой");
        // Агент ни при чём: бутылка в аплаенсе жива, Down не уходил.
        assert!(fake.journal.downs().is_empty());
    }

    /// `read_config` принимает плоское имя конфига в профиле
    /// (`Store::save` нормализует его в `<id>.conf`).
    #[test]
    fn read_config_accepts_plain_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let bottles = tmp.path().join("bottles");
        std::fs::create_dir_all(&bottles).unwrap();
        std::fs::write(bottles.join("office-a.conf"), "wg-office-a").unwrap();

        let vpn = VpnConfig::Env {
            protocol: TunnelProtocol::WireGuard,
            config: "office-a.conf".to_string(),
        };
        assert_eq!(
            read_config(tmp.path(), &vpn).expect("плоское имя читается"),
            "wg-office-a"
        );
    }

    /// Имя конфига из профиля валидируется `^[A-Za-z0-9._-]+$` до
    /// каких-либо обращений к файловой системе: профиль — рукописный
    /// toml на диске, и без барьера `../..` читал бы произвольные файлы
    /// хоста и отправлял их содержимое в аплаенс (ledger-минор T16).
    #[test]
    fn read_config_rejects_traversal_and_absolute_names() {
        // «..» и «.» проходят посимвольный фильтр, но являются
        // traversal-намерением — отвергаются явно.
        for name in ["../../etc/passwd", "/abs/path", "..", "."] {
            let vpn = VpnConfig::Env {
                protocol: TunnelProtocol::WireGuard,
                config: name.to_string(),
            };
            let err = read_config(Path::new("/data"), &vpn).expect_err("имя должно быть отклонено");
            assert!(
                err.contains("недопустимое имя файла конфига"),
                "отказ обязан называть барьер, для {name:?}: {err}"
            );
        }
    }
}
