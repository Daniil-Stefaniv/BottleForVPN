//! Жизненный цикл env-бутылки: оркестрация по спеке §7.1.
//!
//! [`BottleManager::start`] резервирует порты на `127.0.0.1`, поднимает
//! бутылку в аплаенсе (`Up` → `Ready`) и перестраивает зарезервированные
//! слушатели на [`serve_forward`] `127.0.0.1:port → vm_ip:port`; `stop`
//! отправляет агенту `Down` и гасит задачи и слушатели. Демон всегда
//! резолвит порты сам (диапазон 49152–65535 при автоподборе) — агенту
//! уходят только конкретные номера, `socks5_port = 0` не отправляется.
//!
//! Протокол v2: `Up` и `Down` несут instance-поколение бутылки (номер
//! поколения маркера `starting`). Агент опускает бутылку только `Down`
//! с сохранным поколением — `Down` проигравшего рестарта получает
//! `STALE_INSTANCE` и не гасит живую бутылку победителя.
//!
//! Контракт конкурентности: runtime-карта живёт под одним мьютексом,
//! лок удерживается только на коротких участках без `await`; долгие
//! операции (соединение с агентом, `Up` с handshake туннеля) выполняются
//! без лока, а результат докладывается вторым коротким локом. Маркер
//! `starting` несёт номер поколения: если за время старта пришёл `stop`
//! (запись вынута) или произошёл перезапуск (запись заменена чужим
//! поколением), сделанное полностью разбирается.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bottle_core::ipc::{BottleState, CliReply, RuntimeState};
use bottle_core::profile::{BottleKind, BottleProfile, TunnelProtocol, VpnConfig};
use bottle_core::protocol::{err, AgentReply, AgentRequest, ForwardPort, UpArgs};
use relay_lib::forward::{serve_forward, TargetAddr};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::appliance::{ensure_appliance, ApplianceHandle, WslRunner};
use crate::error::DaemonError;
use crate::host_manager::{HostManager, HostParts, HostTiming, BOTTLE_NOT_RUNNING};
use crate::store::Store;

/// Нижняя граница диапазона автоподбора портов (dynamic, RFC 6335).
pub const PORT_DYNAMIC_MIN: u16 = 49152;
/// Верхняя граница диапазона автоподбора портов.
pub const PORT_DYNAMIC_MAX: u16 = 65535;
/// Попыток `bind :0`, прежде чем сдаться: на системах с более широким
/// диапазоном ephemeral-портов не каждая попытка попадает в 49152–65535.
const PORT_DYNAMIC_ATTEMPTS: usize = 1000;
/// Сколько [`RealAgentConnect`] ждёт соединения с агентом после wake
/// (агент стартует при первом обращении к VM; спека §4.3 — ≤30 с).
pub const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Точка подключения к агенту аплаенса; инъекция для тестов.
///
/// Каждая операция жизненного цикла (`start`/`stop`) открывает своё
/// соединение: `connect` дешёвый (пока жива хоть одна бутылка, VM не
/// гаснет), агент обслуживает последовательных клиентов.
#[async_trait]
pub trait AgentConnect: Send + Sync {
    /// Устанавливает соединение с агентом (handshake включён).
    async fn connect(&self) -> Result<ApplianceHandle, DaemonError>;
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
    /// Счётчик поколений маркеров `starting` (см. [`Runtime::generation`]).
    generations: AtomicU64,
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
        Self {
            store,
            connect,
            host: Arc::new(host),
            runtime: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
        }
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
    pub async fn start(&self, id: &str) -> Result<CliReply, DaemonError> {
        let profile = self.store.load(id)?;
        if profile.kind == BottleKind::Host {
            // Спека §7.2 (host-бутылки) — жизненный цикл в host-менеджере.
            return self.host.start(&profile).await;
        }

        // Фаза 1: пометка starting; активная бутылка — no-op (RF#3).
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
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
                    listeners: None,
                    tasks: JoinSet::new(),
                },
            );
        }

        // Фаза 2: длинные операции без лока; instance-поколение этого
        // старта сопровождает Up (агент сохранит его с бутылкой).
        let outcome = self.provision(&profile, generation).await;

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
                        listeners: Some(made.listeners),
                        tasks: made.tasks,
                    },
                );
                drop(runtime);
                tracing::info!(id, "бутылка поднята");
                if let Some(command) = &profile.expose.launch {
                    spawn_launch(command);
                }
                Ok(CliReply::Started { id: id.to_string() })
            }
            Ok(mut made) => {
                // Гонку выиграл stop (записи нет) или перезапуск (в карте
                // запись чужого, более нового поколения): разобрать всё
                // сделанное ЭТИМ запуском — задачи, слушатели — и дождаться
                // освобождения портов. `Down` агенту отправляем, только
                // когда записи нет: после перезапуска под этим id уже
                // живёт новая бутылка, и наш `Down` погасил бы её.
                let superseded_by_restart = runtime.get(id).is_some();
                drop(runtime);
                made.tasks.abort_all();
                while made.tasks.join_next().await.is_some() {
                    // Результат (в т.ч. cancellation) не важен — ждём
                    // завершения, чтобы порты освободились до ответа.
                }
                drop(made.listeners);
                if superseded_by_restart {
                    tracing::warn!(id, "запуск отменён перезапуском: ресурсы освобождены");
                } else if let Err(error) = self.send_down(id, generation).await {
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
        // Маршрутизация по kind профиля (профиль недоступен — путь env
        // ответит «не запущена»).
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
            return Ok(CliReply::Error {
                message: BOTTLE_NOT_RUNNING.to_string(),
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

    /// Фаза 2 `start`: резерв портов, чтение конфига, `Up` → `Ready`,
    /// задачи проброса. При ошибке всё сделанное освобождается
    /// (резерв-слушатели дропаются — порты свободны). `instance` —
    /// поколение этого старта, оно сопровождает `Up` и компенсирующий
    /// `Down` (агент хранит его с бутылкой).
    async fn provision(
        &self,
        profile: &BottleProfile,
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

        // Up; PORT_BUSY → перевыбор динамических портов (фиксированные
        // бинды сохраняются — их порт изменить нельзя) и повтор ровно
        // один раз (RF#5). Повтор — то же поколение: instance у обеих
        // попыток одинаков.
        let selection = match handle
            .send(up_args(
                profile,
                protocol,
                config.clone(),
                &selection,
                instance,
            )?)
            .await
        {
            Ok(AgentReply::Ready { .. }) => selection,
            Ok(AgentReply::Error { code, .. }) if code == err::PORT_BUSY => {
                tracing::warn!(id = %profile.id, "агент сообщил PORT_BUSY: перевыбор портов");
                let retry = retry_ports(profile, selection)?;
                match handle
                    .send(up_args(profile, protocol, config, &retry, instance)?)
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
        if let Err(reason) = spawn_forwards(&mut tasks, &selection, vm_ip) {
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
        BottleState::Starting | BottleState::Up | BottleState::Degraded | BottleState::Stopping
    )
}

/// Запускает команду `expose.launch` после перехода в `up` (спека §7.1);
/// вывод не ждём, ошибка запуска — только журнал.
fn spawn_launch(command: &str) {
    match std::process::Command::new(command).spawn() {
        Ok(child) => tracing::info!(command, pid = child.id(), "launch запущен"),
        Err(error) => tracing::warn!(command, %error, "не удалось запустить launch"),
    }
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

/// Занимает свободный порт динамического диапазона: bind :0, пока выпавший
/// порт не окажется в 49152–65535 (порты вне диапазона освобождаются).
fn reserve_dynamic_port() -> Result<StdTcpListener, String> {
    for _ in 0..PORT_DYNAMIC_ATTEMPTS {
        let listener = StdTcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("не удалось занять слушателя: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        if (PORT_DYNAMIC_MIN..=PORT_DYNAMIC_MAX).contains(&port) {
            set_nonblocking(&listener, port)?;
            return Ok(listener);
        }
        drop(listener);
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
/// агент получает текст, а не имя файла.
fn read_config(data_root: &Path, vpn: &VpnConfig) -> Result<String, String> {
    let VpnConfig::Env { config, .. } = vpn else {
        return Err("конфиг есть только у env-бутылок".to_string());
    };
    let path = data_root.join("bottles").join(config);
    std::fs::read_to_string(&path)
        .map_err(|error| format!("не удалось прочитать конфиг {}: {error}", path.display()))
}

/// Собирает запрос `Up` из профиля, выбранного резерва портов и
/// instance-поколения старта (агент сохранит его с бутылкой).
fn up_args(
    profile: &BottleProfile,
    protocol: TunnelProtocol,
    config: String,
    selection: &HostListeners,
    instance: u64,
) -> Result<AgentRequest, String> {
    Ok(AgentRequest::Up(UpArgs {
        id: profile.id.clone(),
        protocol,
        config,
        dns: profile.expose.dns,
        socks5_port: selection.socks5_port()?,
        forwards: selection.forward_specs(),
        instance,
    }))
}

/// Поднимает задачи проброса для всего резерва: SOCKS5 + пробросы,
/// каждый `127.0.0.1:port → vm_ip:port` (relay агента слушает те же
/// порты на VM).
fn spawn_forwards(
    tasks: &mut JoinSet<()>,
    selection: &HostListeners,
    vm_ip: Ipv4Addr,
) -> Result<(), String> {
    spawn_forward(tasks, &selection.socks5, vm_ip, selection.socks5_port()?)?;
    for (spec, listener) in &selection.forwards {
        spawn_forward(tasks, listener, vm_ip, spec.port)?;
    }
    Ok(())
}

/// Ставит слушателя на проброс `127.0.0.1:port → vm_ip:port`
/// (relay агента слушает тот же порт на VM).
fn spawn_forward(
    tasks: &mut JoinSet<()>,
    listener: &StdTcpListener,
    vm_ip: Ipv4Addr,
    port: u16,
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
    tasks.spawn(async move {
        if let Err(error) = serve_forward(dup, target).await {
            tracing::warn!(port, %error, "сервер проброса остановлен");
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
    }

    /// Журнал фейк-агента: полученные `Up`/`Down` и живые relay.
    #[derive(Clone, Default)]
    struct AgentJournal {
        ups: Arc<Mutex<Vec<UpArgs>>>,
        downs: Arc<Mutex<Vec<(String, u64)>>>,
        relays: Arc<Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>,
        /// Живые бутылки по протоколу v2: id → instance-поколение `Up`,
        /// создавшего бутылку. Регистрируются при ответе `Ready` (как
        /// в настоящем агенте — реестр до ответа), `Down` проверяет
        /// поколение: чужое → `STALE_INSTANCE`, бутылка не трогается.
        live: Arc<Mutex<HashMap<String, u64>>>,
    }

    impl AgentJournal {
        fn ups(&self) -> Vec<UpArgs> {
            self.ups.lock().unwrap().clone()
        }

        fn downs(&self) -> Vec<(String, u64)> {
            self.downs.lock().unwrap().clone()
        }

        fn remember_relays(&self, id: &str, handles: Vec<tokio::task::JoinHandle<()>>) {
            // Релеи накапливаются, а не заменяются: при гонке restart
            // Up-ы одного id завершаются в произвольном порядке, и поздний
            // Ready проигравшего не должен гасить релеи живого победителя.
            // Down гасит все релеи id разом.
            self.relays
                .lock()
                .unwrap()
                .entry(id.to_string())
                .or_default()
                .extend(handles);
        }

        fn drop_relays(&self, id: &str) {
            if let Some(handles) = self.relays.lock().unwrap().remove(id) {
                for handle in handles {
                    handle.abort();
                }
            }
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
    /// накапливаются и гасятся только `Down` своего поколения.
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
    }

    impl FakeAgent {
        async fn spawn(behavior: UpBehavior) -> FakeAgent {
            let vm_ip = local_ipv4();
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind control");
            let addr = listener.local_addr().expect("local_addr");
            let journal = AgentJournal::default();
            tokio::spawn(run_agent(listener, vm_ip, behavior, journal.clone()));
            FakeAgent { addr, journal }
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
            let mut stream = TcpStream::connect(self.addr)
                .await
                .map_err(|e| DaemonError::Appliance(format!("фейк-агент недоступен: {e}")))?;
            stream.set_nodelay(true).expect("nodelay");
            let vm_ip = client_handshake(&mut stream).await;
            Ok(ApplianceHandle::wrap(vm_ip, stream))
        }
    }

    /// `Hello` → `HelloAck` на «сыром» потоке; возвращает vm_ip из ответа.
    async fn client_handshake(stream: &mut TcpStream) -> Ipv4Addr {
        let hello = AgentRequest::Hello {
            version: PROTOCOL_VERSION,
            token: "test-token".to_string(),
        };
        stream
            .write_all(&encode_frame(&hello))
            .await
            .expect("отправка Hello");

        let mut buf = BytesMut::new();
        loop {
            if let Some(reply) = decode_frame(&mut buf).expect("кадр HelloAck") {
                let AgentReply::HelloAck { vm_ip, .. } = reply else {
                    panic!("ожидали HelloAck, получили {reply:?}");
                };
                return vm_ip;
            }
            let n = stream.read_buf(&mut buf).await.expect("чтение HelloAck");
            assert!(n > 0, "фейк-агент закрыл соединение до HelloAck");
        }
    }

    /// Принимает соединения демона; сессии обслуживаются конкурентно.
    async fn run_agent(
        listener: TcpListener,
        vm_ip: Ipv4Addr,
        behavior: UpBehavior,
        journal: AgentJournal,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
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
                AgentRequest::Down { id, instance } => down_reply(id, instance, &journal),
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
                journal.drop_relays(&id);
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
            journal.remember_relays(&args.id, handles);
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

    #[tokio::test]
    async fn start_answers_ready_socks5_works_and_status_is_up() {
        let (_tmp, store) = store_with(&env_profile("office-a", "ignored-by-save.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _dispatch_store) = manager_with(store.data_root(), fake.clone());

        let reply = manager.start("office-a").await.expect("start ок");
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
        // instance-поколением старта (первый start — поколение 0).
        let ups = fake.journal.ups();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].id, "office-a");
        assert_eq!(ups[0].protocol, TunnelProtocol::WireGuard);
        assert_eq!(ups[0].config, "wg-office-a", "конфиг передан содержимым");
        assert_eq!(ups[0].dns, Some("10.20.30.1".parse().unwrap()));
        assert_ne!(ups[0].socks5_port, 0, "нулевые порты не отправляются");
        assert_ne!(ups[0].forwards[0].port, 0, "нулевые порты не отправляются");
        assert_eq!(ups[0].instance, 0, "Up несёт instance-поколение");
    }

    #[tokio::test]
    async fn second_start_on_up_is_noop_with_single_bottle() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        manager.start("office-a").await.expect("первый start");
        let reply = manager
            .start("office-a")
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

        let reply = manager.start("office-a").await.expect("start после ретрая");
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
        let reply = manager.start("fixed-a").await.expect("start после ретрая");
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

        let reply = manager.start("fixed-b").await.expect("ответ, не сбой");
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
            .start("office-a")
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

        // Stop незапущенной бутылки — понятная ошибка.
        let reply = manager.stop("office-a").await.expect("stop без паники");
        assert_eq!(
            reply,
            CliReply::Error {
                message: "бутылка не запущена".to_string()
            }
        );

        manager.start("office-a").await.expect("start");
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
        assert_eq!(
            fake.journal.downs(),
            vec![("office-a".to_string(), 0)],
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

        manager.start("office-a").await.expect("start"); // поколение 0
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
            vec![("office-a".to_string(), 999), ("office-a".to_string(), 0)],
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
            tokio::spawn(async move { manager.start("office-a").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await; // start1 уже в фазе Up
        manager
            .stop("office-a")
            .await
            .expect("stop во время старта");
        let reply = manager.start("office-a").await.expect("restart");
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

        // Down отправлял только stop: teardown проигравшего НЕ гасит
        // бутылку победителя (под тем же id в аплаенсе уже новая).
        assert_eq!(
            fake.journal.downs(),
            vec![("office-a".to_string(), 0)],
            "Down только от stop, с поколением проигравшего старта"
        );
    }

    #[tokio::test]
    async fn stop_during_start_tears_down_and_sends_down() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::DelayedFirstUp(Duration::from_millis(300))).await;
        let (manager, _store) = manager_with(store.data_root(), fake.clone());

        let first = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.start("office-a").await })
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
            vec![("office-a".to_string(), 0), ("office-a".to_string(), 0)],
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

        let reply = manager.start("corp-vpn").await.expect("ответ, не сбой");
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

        let reply = manager.start("office-a").await.expect("ответ, не сбой");
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

    #[tokio::test]
    async fn dispatch_wires_start_stop_status_and_show_runtime_to_manager() {
        let (_tmp, store) = store_with(&env_profile("office-a", "wg.conf"));
        let fake = FakeAgent::spawn(UpBehavior::Ready).await;
        let (manager, store) = manager_with(store.data_root(), fake);

        // Start через диспетчер.
        let reply = crate::pipe::dispatch(
            &store,
            Some(&manager),
            bottle_core::ipc::CliRequest::Start {
                id: "office-a".to_string(),
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
        let reply =
            crate::pipe::dispatch(&store, Some(&manager), bottle_core::ipc::CliRequest::Status)
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
        let reply =
            crate::pipe::dispatch(&store, Some(&manager), bottle_core::ipc::CliRequest::Status)
                .await;
        assert_eq!(reply, CliReply::Status { bottles: vec![] });
    }
}
