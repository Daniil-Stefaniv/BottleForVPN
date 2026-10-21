//! RPC-протокол CLI и демона: команды пользователя и снимки состояния.
//!
//! Сообщения передаются кадрами [`crate::frame`] (u32 BE длина + JSON)
//! по именованному каналу [`DAEMON_PIPE`]. Перечисления сериализуются
//! внешней пометкой serde по умолчанию (например, `{"Start":{"id":"a"}}`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::profile::{BottleKind, BottleProfile};
use crate::protocol::ForwardPort;

/// Имя канала демона (Windows named pipe).
pub const DAEMON_PIPE: &str = r"\\.\pipe\bottled";

/// Имя UDS-сокета демона внутри каталога данных (macOS/Unix, M4 D3).
pub const DAEMON_SOCK_FILE: &str = "daemon.sock";

/// Путь UDS-сокета демона: `<data>/daemon.sock`. Каталог данных уже
/// 0700 (Store), файл сокета — 0600 (прав достаточно, токен не нужен).
pub fn daemon_socket_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(DAEMON_SOCK_FILE)
}

/// Каталог данных по умолчанию (CLI и демон считают одинаково):
/// локальный каталог данных пользователя + `BottleForApps`.
pub fn default_data_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|base| base.join("BottleForApps"))
}

/// UDS-клиент CLI↔демона (macOS/Unix, M4 Task 7): подключение с
/// retry-окном (демон поднимается — ENOENT/ECONNREFUSED пережидается),
/// один запрос — один ответ кадрами LengthJsonCodec, таймаут ответа.
///
/// Ошибки — русские строки, совпадающие по смыслу с named-pipe
/// транспортом Windows (ECONNREFUSED ≈ «демон не запущен»).
#[cfg(unix)]
pub async fn uds_exchange(
    socket: &std::path::Path,
    request: &CliRequest,
    reply_timeout: std::time::Duration,
    connect_window: std::time::Duration,
) -> Result<CliReply, String> {
    use tokio::io::{split, AsyncWriteExt};
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Encoder, FramedRead};

    use crate::frame::LengthJsonCodec;

    let deadline = tokio::time::Instant::now() + connect_window;
    let stream = loop {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => break stream,
            Err(error) => {
                let stale = matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                );
                if !stale || tokio::time::Instant::now() >= deadline {
                    return Err(
                        "Не удалось подключиться к демону BottleForApps — запущен ли bottled?"
                            .to_string(),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };

    let (read_half, mut write_half) = split(stream);
    let mut request_codec = LengthJsonCodec::<CliRequest>::new();
    let mut buf = bytes::BytesMut::new();
    request_codec
        .encode(request.clone(), &mut buf)
        .map_err(|error| format!("Ошибка обмена с демоном: {error}"))?;
    write_half
        .write_all(&buf)
        .await
        .map_err(|error| format!("Ошибка обмена с демоном: {error}"))?;

    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<CliReply>::new());
    match tokio::time::timeout(reply_timeout, reader.next()).await {
        Ok(Some(Ok(reply))) => Ok(reply),
        Ok(Some(Err(error))) => Err(format!("Ошибка обмена с демоном: {error}")),
        Ok(None) => Err("Демон закрыл соединение без ответа".to_string()),
        Err(_) => Err(format!("Демон не ответил за {} с", reply_timeout.as_secs())),
    }
}

/// Запрос CLI к демону.
///
/// `Create` крупнее остальных вариантов из-за встроенного профиля;
/// это редкое сообщение control-канала, боксирование не выполняется
/// (тот же прецедент, что у `CliReply::Show`).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliRequest {
    /// Список бутылок из хранилища (с известным состоянием).
    List,
    /// Показать профиль и/или рантайм-состояние бутылки.
    Show { id: String },
    /// Поднять бутылку. `credentials` — логин/пароль для OpenVPN
    /// `auth-user-pass`, если демон запросил их ответом
    /// [`CliReply::NeedCredentials`]; не хранятся, живут только в рамках
    /// этого запуска (передаются агенту в `Up`).
    Start {
        id: String,
        /// Креды VPN этой сессии запуска; `None` — без авторизации.
        #[serde(default)]
        credentials: Option<VpnCredentials>,
    },
    /// Опустить бутылку.
    Stop { id: String },
    /// Рантайм-состояние всех поднятых бутылок.
    Status,
    /// Экспортировать журналы в ZIP; `dest` — путь пользователя,
    /// `None` — имя по умолчанию в каталоге данных.
    LogsExport { dest: Option<PathBuf> },
    /// Диагностика окружения (WSL, аплаенс, служба, профили, порты).
    Doctor,
    /// Создать бутылку из профиля; `force` разрешает перезапись
    /// существующего id (профиль и конфиг пользователя теряются).
    Create {
        /// Профиль (для env `vpn.config` — путь к исходному конфигу,
        /// демон копирует его в хранилище).
        profile: BottleProfile,
        /// Перезаписать существующий профиль этого id.
        force: bool,
    },
    /// Удалить бутылку (отказ, если она активна).
    Delete { id: String },
    /// Пере-импорт аплаенса из rootfs (отказ при живых бутылках).
    /// `force` — полный сброс: все бутылки останавливаются принудительно,
    /// WSL гасится целиком (`wsl --shutdown`), регистрация дистрибуции
    /// снимается с ретраями, токен пересоздаётся.
    Upgrade {
        /// Полный сброс (остановка бутылок и WSL).
        #[serde(default)]
        force: bool,
    },
    /// Остановить все бутылки и завершить процесс демона (кнопка
    /// «Остановить всё» в GUI / `bottle daemon stop`).
    ShutdownDaemon,
    /// Снять сиротские маршруты через службу (bottle routes cleanup).
    RoutesCleanup,
}

/// Логин/пароль VPN для одного запуска бутылки (OpenVPN
/// `auth-user-pass`). Передаются от CLI/GUI демону и далее агенту в
/// `Up`; на диск не пишутся — запрашиваются при каждом подключении.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnCredentials {
    /// Логин VPN.
    pub username: String,
    /// Пароль VPN.
    pub password: String,
}

/// Ответ демона CLI.
///
/// `Show` крупнее остальных вариантов из-за встроенного профиля;
/// это редкое сообщение control-канала, поэтому размер оправдан
/// и боксирование не выполняется.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliReply {
    /// Ответ на `List`: бутылки хранилища.
    List { bottles: Vec<BottleSummary> },
    /// Ответ на `Show`: профиль с диска и/или рантайм-состояние;
    /// отсутствующая половина — `None`.
    Show {
        profile: Option<BottleProfile>,
        runtime: Option<RuntimeState>,
    },
    /// Бутылка поднята (ответ на `Start`).
    Started { id: String },
    /// Ответ на `Start`: конфиг OpenVPN требует `auth-user-pass`, а креды
    /// в запросе не переданы — клиент должен спросить логин/пароль у
    /// пользователя (CLI — скрытым вводом, GUI — диалогом) и повторить
    /// `Start` с `credentials`.
    NeedCredentials { id: String },
    /// Бутылка опущена (ответ на `Stop`); `warning` — предупреждение
    /// о неочищенных эффектах (host-бутылка: VPN-клиент остаётся
    /// работать — спека §7.2, шаг 6). Отсутствующее поле в старом
    /// кадре десериализуется как `None`.
    Stopped { id: String, warning: Option<String> },
    /// Ответ на `Status`.
    Status { bottles: Vec<RuntimeState> },
    /// Ответ на `LogsExport`: путь к созданному ZIP-архиву.
    Path(String),
    /// Ответ на `Doctor`: строки проверок.
    DoctorReport { items: Vec<CheckItem> },
    /// Ответ на `Create`: бутылка создана (профиль и конфиг записаны).
    Created { id: String },
    /// Ответ на `Delete`: файлы профиля удалены; `warning` — например,
    /// конфигурационного файла уже не было.
    Deleted { id: String, warning: Option<String> },
    /// Ответ на `Upgrade`: версия аплаенса до (`None` — не подключались
    /// в этой сессии) и после пере-импорта.
    Upgraded {
        previous: Option<String>,
        current: String,
    },
    /// Успех без данных (`routes cleanup`); `warning` — например,
    /// служба маршрутов не установлена.
    Done { warning: Option<String> },
    /// Ошибка выполнения команды.
    Error { message: String },
}

/// Строка списка бутылок: профиль с диска + известное состояние, если есть.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottleSummary {
    /// Идентификатор бутылки.
    pub id: String,
    /// Человекочитаемое имя.
    pub name: String,
    /// Тип бутылки (env/host).
    pub kind: BottleKind,
    /// Текущее состояние (`up`/...); `None` = не запущена/неизвестно.
    pub state: Option<String>,
}

/// Состояние бутылки в рантайме.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BottleState {
    /// Поднимается: туннель/SOCKS5/пробросы ещё настраиваются.
    Starting,
    /// Поднята и работает.
    Up,
    /// Работает с деградацией: умерла relay-задача или агент аплаенса
    /// не отвечает — туннель/часть пробросов недоступна, локальные
    /// слушатели и сессия живут (T11, спека §9).
    Degraded { reason: String },
    /// Поднятие или работа завершились ошибкой.
    Failed { reason: String },
    /// Останавливается.
    Stopping,
    /// Остановлена.
    Stopped,
}

impl BottleState {
    /// Каноническое имя состояния для проводного формата и отображения.
    pub fn as_str(&self) -> &'static str {
        match self {
            BottleState::Starting => "starting",
            BottleState::Up => "up",
            BottleState::Degraded { .. } => "degraded",
            BottleState::Failed { .. } => "failed",
            BottleState::Stopping => "stopping",
            BottleState::Stopped => "stopped",
        }
    }
}

/// Рантайм-состояние одной бутылки (живые порты и состояние).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    /// Идентификатор бутылки.
    pub id: String,
    /// Состояние.
    pub state: BottleState,
    /// Назначенный порт SOCKS5 на 127.0.0.1; `None` = ещё не назначен.
    pub socks5: Option<u16>,
    /// Активные TCP-пробросы с назначенными портами слушателя.
    pub forwards: Vec<ForwardPort>,
}

/// Статус проверки `doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    /// Проверка прошла.
    Ok,
    /// Настораживающее, но допустимое состояние (portable без службы,
    /// аплаенс ещё не импортирован…).
    Warn,
    /// Функциональность недоступна без действий пользователя.
    Fail,
}

/// Одна строка отчёта `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckItem {
    /// Код проверки (`wsl`, `appliance`, `service`, `agent`,
    /// `profiles`, `ports`).
    pub code: String,
    /// Итог проверки.
    pub status: CheckStatus,
    /// Русское пояснение (что не так и что делать).
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Протокол GUI↔демон (0.3, спека §5): GUI — хозяин хранилища, демон —
// исполнитель без диска. Control-соединение: запрос/ответ GuiRequest/
// GuiReply; event-соединение (Subscribe): поток DaemonEvent.
// ---------------------------------------------------------------------------

/// Код ошибки авторизации: неверный или отсутствующий токен GUI.
pub const AUTH_ERR: &str = "AUTH";

/// Профиль бутылки целиком: то, что GUI хранит в БД и передаёт демону.
/// `config_text` — полный текст VPN-конфига (env-бутылки); `credentials` —
/// сохранённые логин/пароль (спека §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullProfile {
    /// Профиль (id, имя, kind, vpn, expose).
    pub profile: BottleProfile,
    /// Текст конфига туннеля; `None` для host-бутылок.
    #[serde(default)]
    pub config_text: Option<String>,
    /// Сохранённые логин/пароль; демон держит только в памяти.
    #[serde(default)]
    pub credentials: Option<VpnCredentials>,
}

/// Запрос GUI к демону (control-соединение).
///
/// До `Hello` с верным токеном демон принимает только служебные
/// `Doctor`/`LogsExport` (нужны CLI `pogrebok`, у которого токена нет);
/// всё остальное отклоняется [`AUTH_ERR`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GuiRequest {
    /// Первое сообщение control-соединения: токен, выданный демону
    /// аргументом `--gui-token` при спавне из GUI.
    Hello { token: String },
    /// Полная синхронизация реестра: все профили + конфиги + креды.
    SyncProfiles { profiles: Vec<FullProfile> },
    /// Точечное обновление/добавление профиля после мутации БД.
    UpsertProfile { profile: Box<FullProfile> },
    /// Точечное удаление профиля; поднятую бутылку удалять нельзя.
    RemoveProfile { id: String },
    /// Поднять бутылку; креды демон уже имеет из синхронизации.
    StartBottle { id: String },
    /// Опустить бутылку.
    StopBottle { id: String },
    /// Корректная остановка демона (кнопка «Остановить демона»,
    /// выход из трея): стоп всех бутылок → выход.
    Shutdown,
    /// Открыть event-поток на этом соединении (после проверки токена
    /// соединение не закрывается — в него пишутся кадры DaemonEvent).
    Subscribe { token: String },
    /// Диагностика окружения; доступна без токена (CLI `pogrebok`).
    Doctor,
    /// Экспорт журналов в ZIP; доступен без токена (CLI `pogrebok`).
    LogsExport { dest: Option<PathBuf> },
    /// Статистика демона для карт состояния в Диагностике.
    Stats,
}

/// Ответ демона GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GuiReply {
    /// Успех без данных.
    Ok,
    /// Ошибка; коды — префиксом в `message` (например, [`AUTH_ERR`]).
    Error { message: String },
    /// Ответ на `Stats`.
    Stats(Box<DaemonStats>),
    /// Ответ на `Doctor`.
    DoctorReport { items: Vec<CheckItem> },
    /// Ответ на `LogsExport`: путь к ZIP.
    Path(String),
}

/// Статистика демона для карт состояния вкладки «Диагностика».
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStats {
    /// PID процесса демона.
    pub pid: u32,
    /// Версия демона.
    pub version: String,
    /// Время старта (RFC3339 UTC).
    pub started_at: String,
    /// Число поднятых бутылок.
    pub active_bottles: u32,
    /// Число профилей в реестре (получено от GUI).
    pub total_profiles: u32,
    /// RSS процесса, байт.
    pub mem_bytes: u64,
    /// Загрузка CPU процесса, %.
    pub cpu_pct: f32,
    /// Состояние WSL-VM (`running`/`stopped`/`unknown`).
    pub wsl_state: String,
    /// Версия апплайенса, если подключался в этой сессии.
    pub appliance_version: Option<String>,
}

/// Событие демона для GUI (event-соединение после `Subscribe`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DaemonEvent {
    /// Изменилось состояние бутылки (замена поллинга `Status`).
    StateChanged {
        id: String,
        /// `up`/`stopped`/`failed`/`degraded`/`unknown`.
        state: String,
        /// Детали (причина failed/degraded); `None` — без подробностей.
        detail: Option<String>,
        /// Порт SOCKS5 на 127.0.0.1, если известен.
        socks5: Option<u16>,
        /// Активные пробросы на 127.0.0.1.
        forwards: Vec<ForwardPort>,
    },
    /// Событие журнала из реестра (спека §6) для живой консоли.
    Log(Box<crate::events::Event>),
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::path::PathBuf;

    use super::*;
    use crate::frame::LengthJsonCodec;
    use crate::profile::{BottleKind, BottleProfile, ExposeConfig, ForwardSpec, VpnConfig};
    use bytes::BytesMut;
    use serde::de::DeserializeOwned;
    use tokio_util::codec::{Decoder, Encoder};

    /// Кодирует и декодирует значение через `LengthJsonCodec` (стиль `Framed`).
    fn round_trip<T>(value: T) -> T
    where
        T: Serialize + Clone + DeserializeOwned,
    {
        let mut codec = LengthJsonCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(value.clone(), &mut buf)
            .expect("encode must succeed for ipc types");
        codec
            .decode(&mut buf)
            .expect("decode must not fail on a well-formed frame")
            .expect("buffer must contain a complete frame")
    }

    fn sample_profile() -> BottleProfile {
        BottleProfile {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            vpn: VpnConfig::Env {
                protocol: crate::profile::TunnelProtocol::WireGuard,
                config: "wg-office-a.conf".to_string(),
            },
            expose: ExposeConfig {
                socks5: 1080,
                forwards: vec![ForwardSpec {
                    listen: 33389,
                    target: "10.20.30.15:3389".to_string(),
                }],
                dns: Some(IpAddr::from([10, 20, 30, 1])),
                launch: Some("mstsc".to_string()),
            },
        }
    }

    fn sample_runtime(id: &str, state: BottleState) -> RuntimeState {
        RuntimeState {
            id: id.to_string(),
            state,
            socks5: Some(1080),
            forwards: vec![crate::protocol::ForwardPort {
                port: 33389,
                target: "10.20.30.15:3389".to_string(),
            }],
        }
    }

    #[test]
    fn daemon_pipe_value_is_the_wire_contract() {
        assert_eq!(DAEMON_PIPE, r"\\.\pipe\bottled");
    }

    #[test]
    fn round_trip_every_request_variant() {
        let requests = vec![
            CliRequest::List,
            CliRequest::Show {
                id: "office-a".to_string(),
            },
            CliRequest::Start {
                id: "office-a".to_string(),
                credentials: None,
            },
            CliRequest::Start {
                id: "office-a".to_string(),
                credentials: Some(VpnCredentials {
                    username: "user@corp".to_string(),
                    password: "s3cret".to_string(),
                }),
            },
            CliRequest::Stop {
                id: "office-a".to_string(),
            },
            CliRequest::Status,
            CliRequest::LogsExport { dest: None },
            CliRequest::LogsExport {
                dest: Some(PathBuf::from(r"C:\tmp\logs.zip")),
            },
            CliRequest::Doctor,
            CliRequest::Create {
                profile: sample_profile(),
                force: false,
            },
            CliRequest::Create {
                profile: sample_profile(),
                force: true,
            },
            CliRequest::Delete {
                id: "office-a".to_string(),
            },
            CliRequest::Upgrade { force: false },
            CliRequest::Upgrade { force: true },
            CliRequest::ShutdownDaemon,
            CliRequest::RoutesCleanup,
        ];
        for req in requests {
            assert_eq!(round_trip(req.clone()), req);
        }
    }

    #[test]
    fn round_trip_every_reply_variant() {
        let summary = |state: Option<&str>| BottleSummary {
            id: "office-a".to_string(),
            name: "Офис А".to_string(),
            kind: BottleKind::Env,
            state: state.map(str::to_string),
        };

        let replies = vec![
            CliReply::List {
                bottles: vec![
                    summary(Some("up")),
                    summary(None),
                    BottleSummary {
                        id: "corp-vpn".to_string(),
                        name: "Корп VPN".to_string(),
                        kind: BottleKind::Host,
                        state: Some("stopped".to_string()),
                    },
                ],
            },
            CliReply::List { bottles: vec![] },
            CliReply::Show {
                profile: Some(sample_profile()),
                runtime: Some(sample_runtime("office-a", BottleState::Up)),
            },
            CliReply::Show {
                profile: Some(sample_profile()),
                runtime: None,
            },
            CliReply::Show {
                profile: None,
                runtime: Some(sample_runtime(
                    "office-a",
                    BottleState::Failed {
                        reason: "туннель не поднялся".to_string(),
                    },
                )),
            },
            CliReply::Show {
                profile: None,
                runtime: None,
            },
            CliReply::Started {
                id: "office-a".to_string(),
            },
            CliReply::NeedCredentials {
                id: "office-a".to_string(),
            },
            CliReply::Stopped {
                id: "office-a".to_string(),
                warning: None,
            },
            CliReply::Stopped {
                id: "corp-vpn".to_string(),
                warning: Some("VPN-клиент оставлен запущенным".to_string()),
            },
            CliReply::Status {
                bottles: vec![
                    sample_runtime("office-a", BottleState::Up),
                    sample_runtime(
                        "corp-vpn",
                        BottleState::Failed {
                            reason: "туннель не поднялся".to_string(),
                        },
                    ),
                    sample_runtime(
                        "office-b",
                        BottleState::Degraded {
                            reason: "агент аплаенса не отвечает".to_string(),
                        },
                    ),
                    RuntimeState {
                        id: "empty".to_string(),
                        state: BottleState::Stopped,
                        socks5: None,
                        forwards: vec![],
                    },
                ],
            },
            CliReply::Status { bottles: vec![] },
            CliReply::Path(r"C:\tmp\bottleforapps-logs-20261004-1200.zip".to_string()),
            CliReply::DoctorReport {
                items: vec![
                    CheckItem {
                        code: "wsl".to_string(),
                        status: CheckStatus::Ok,
                        detail: "WSL включён, дистрибуций: 2".to_string(),
                    },
                    CheckItem {
                        code: "service".to_string(),
                        status: CheckStatus::Warn,
                        detail: "Служба маршрутов не установлена".to_string(),
                    },
                    CheckItem {
                        code: "agent".to_string(),
                        status: CheckStatus::Fail,
                        detail: "агент не отвечает".to_string(),
                    },
                ],
            },
            CliReply::Created {
                id: "office-a".to_string(),
            },
            CliReply::Deleted {
                id: "office-a".to_string(),
                warning: None,
            },
            CliReply::Deleted {
                id: "office-a".to_string(),
                warning: Some("файл конфигурации отсутствовал".to_string()),
            },
            CliReply::Upgraded {
                previous: Some("0.1.0".to_string()),
                current: "0.2.0".to_string(),
            },
            CliReply::Upgraded {
                previous: None,
                current: "0.2.0".to_string(),
            },
            CliReply::Done { warning: None },
            CliReply::Done {
                warning: Some("служба маршрутов не установлена".to_string()),
            },
            CliReply::Error {
                message: "bottle not found".to_string(),
            },
        ];
        for reply in replies {
            assert_eq!(round_trip(reply.clone()), reply);
        }
    }

    #[test]
    fn bottle_state_as_str_matches_wire_contract() {
        assert_eq!(BottleState::Starting.as_str(), "starting");
        assert_eq!(BottleState::Up.as_str(), "up");
        assert_eq!(
            BottleState::Degraded {
                reason: "агент не отвечает".to_string()
            }
            .as_str(),
            "degraded"
        );
        assert_eq!(
            BottleState::Failed {
                reason: "туннель не поднялся".to_string()
            }
            .as_str(),
            "failed"
        );
        assert_eq!(BottleState::Stopping.as_str(), "stopping");
        assert_eq!(BottleState::Stopped.as_str(), "stopped");
    }

    #[test]
    fn failed_state_round_trips_with_reason() {
        let state = BottleState::Failed {
            reason: "туннель не поднялся".to_string(),
        };
        let json = serde_json::to_string(&state).expect("serialize BottleState");
        assert_eq!(json, r#"{"Failed":{"reason":"туннель не поднялся"}}"#);
        assert_eq!(round_trip(state.clone()), state);
    }

    /// `Degraded` несёт причину на проводе (T11): машина состояний
    /// показывает, чем именно деградирована бутылка.
    #[test]
    fn degraded_state_round_trips_with_reason() {
        let state = BottleState::Degraded {
            reason: "агент аплаенса не отвечает".to_string(),
        };
        let json = serde_json::to_string(&state).expect("serialize BottleState");
        assert_eq!(
            json,
            r#"{"Degraded":{"reason":"агент аплаенса не отвечает"}}"#
        );
        assert_eq!(round_trip(state.clone()), state);
    }

    /// Поле `warning` в `Stopped` опционально на проводе: кадр старой
    /// формы (до появления поля) читается как `warning: None` —
    /// смешанные версии CLI/демона не ломают канал.
    #[test]
    fn stopped_without_warning_field_deserializes_as_none() {
        let reply: CliReply =
            serde_json::from_str(r#"{"Stopped":{"id":"office-a"}}"#).expect("старая форма кадра");
        assert_eq!(
            reply,
            CliReply::Stopped {
                id: "office-a".to_string(),
                warning: None
            }
        );
    }

    /// Поле `credentials` в `Start` опционально на проводе: кадр старой
    /// формы (без поля) читается как `credentials: None`.
    #[test]
    fn start_without_credentials_field_deserializes_as_none() {
        let request: CliRequest =
            serde_json::from_str(r#"{"Start":{"id":"office-a"}}"#).expect("старая форма кадра");
        assert_eq!(
            request,
            CliRequest::Start {
                id: "office-a".to_string(),
                credentials: None
            }
        );
    }

    /// До M2 `Degraded` был юнит-вариантом и лежал на проводе кадром
    /// `"Degraded"`; теперь несёт причину (см. тест
    /// `degraded_state_round_trips_with_reason`). Старый кадр НЕ
    /// десериализуется — осознанный разрыв M2: CLI и демон ставятся
    /// вместе, смешанных версий на канале не бывает. Разрыв закреплён
    /// тестом (обратный прецедент — добавление опционального поля в
    /// `stopped_without_warning_field_deserializes_as_none`).
    #[test]
    fn degraded_old_unit_variant_frame_is_deliberate_m2_break() {
        let result = serde_json::from_str::<BottleState>("\"Degraded\"");
        assert!(
            result.is_err(),
            "старая без-field форма кадра Degraded обязана отвергаться: {:?}",
            result
        );
    }

    /// Протокол GUI↔демон (0.3): каждый вариант GuiRequest/GuiReply/
    /// DaemonEvent переживает кадр LengthJsonCodec без потерь.
    #[test]
    fn gui_protocol_round_trip() {
        let full = FullProfile {
            profile: sample_profile(),
            config_text: Some("[Interface]\nPrivateKey = …".to_string()),
            credentials: Some(VpnCredentials {
                username: "ivan".to_string(),
                password: "s3cret".to_string(),
            }),
        };
        let requests = vec![
            GuiRequest::Hello {
                token: "ab".repeat(32),
            },
            GuiRequest::SyncProfiles {
                profiles: vec![full.clone()],
            },
            GuiRequest::UpsertProfile {
                profile: Box::new(full.clone()),
            },
            GuiRequest::RemoveProfile {
                id: "office-a".to_string(),
            },
            GuiRequest::StartBottle {
                id: "office-a".to_string(),
            },
            GuiRequest::StopBottle {
                id: "office-a".to_string(),
            },
            GuiRequest::Shutdown,
            GuiRequest::Subscribe {
                token: "ab".repeat(32),
            },
            GuiRequest::Doctor,
            GuiRequest::LogsExport { dest: None },
            GuiRequest::LogsExport {
                dest: Some(PathBuf::from("C:\\logs.zip")),
            },
            GuiRequest::Stats,
        ];
        for req in requests {
            assert_eq!(round_trip(req.clone()), req);
        }

        let replies = vec![
            GuiReply::Ok,
            GuiReply::Error {
                message: "AUTH: сначала Hello с токеном".to_string(),
            },
            GuiReply::Stats(Box::new(DaemonStats {
                pid: 4217,
                version: "0.3.0".to_string(),
                started_at: "2026-10-21T19:04:00.000Z".to_string(),
                active_bottles: 1,
                total_profiles: 5,
                mem_bytes: 84 * 1024 * 1024,
                cpu_pct: 0.3,
                wsl_state: "running".to_string(),
                appliance_version: Some("v3".to_string()),
            })),
            GuiReply::DoctorReport { items: vec![] },
            GuiReply::Path("C:\\logs.zip".to_string()),
        ];
        for reply in replies {
            assert_eq!(round_trip(reply.clone()), reply);
        }

        let events = vec![
            DaemonEvent::StateChanged {
                id: "office-a".to_string(),
                state: "up".to_string(),
                detail: None,
                socks5: Some(51820),
                forwards: vec![crate::protocol::ForwardPort {
                    port: 53389,
                    target: "10.20.30.15:3389".to_string(),
                }],
            },
            DaemonEvent::StateChanged {
                id: "office-a".to_string(),
                state: "failed".to_string(),
                detail: Some("handshake timeout".to_string()),
                socks5: None,
                forwards: vec![],
            },
            DaemonEvent::Log(Box::new(crate::events::Event {
                ts: "2026-10-21T21:12:04.331Z".to_string(),
                level: crate::events::Level::Info,
                src: crate::events::Src::Daemon,
                event: "bottle.up".to_string(),
                desc: "office-a поднята (socks5 :51820)".to_string(),
                ctx: Default::default(),
            })),
        ];
        for event in events {
            assert_eq!(round_trip(event.clone()), event);
        }
    }

    #[test]
    fn auth_err_constant_matches_wire_contract() {
        assert_eq!(AUTH_ERR, "AUTH");
    }
}
