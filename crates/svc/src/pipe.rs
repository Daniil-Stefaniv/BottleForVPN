//! Pipe-сервер службы маршрутов: named pipe [`SVC_PIPE`], кадры
//! `LengthJsonCodec<SvcRequest/SvcReply>`, диспетчер [`dispatch`]
//! (задача 6).
//!
//! Клиенты обслуживаются по одному (демон один; фоновый поток на
//! соединение не нужен): после отключения создаётся свежий экземпляр
//! канала. Каждое соединение — сессия демона: `Hello{daemon_pid}`
//! выделяет монотонный [`SessionId`] и привязывает его к соединению;
//! повторный `Hello` на живом соединении закрывает прежнюю сессию
//! (демон перезапустился) и открывает новую. Когда соединение
//! закрывается — демон умер или отключился — маршруты сессии
//! удаляются по принципу OS-first: сначала из ОС, после успеха — из
//! журнала и live-set; провал остаётся в live-set и ОС, и его подберёт
//! сверка при следующем старте службы (спека §9: маршруты не
//! переживают смерть демона).

use bottle_core::svcrpc::{
    svc_err, validate_gateway, validate_route, RouteEntry, SvcReply, SvcRequest,
};
use tokio::sync::Mutex;

use crate::journal::{RouteStore, SessionId};
use crate::routes::{run_ps, RouteCmd};

/// Предельная длина псевдонима интерфейса (защита в глубину).
/// Windows InterfaceAlias бывает длиннее 64 символов (составные имена
/// вроде «vEthernet (WSL (Hyper-V firewall))»), поэтому лимит — 128.
pub const MAX_IFACE_ALIAS_LEN: usize = 128;

/// SDDL-дескриптор безопасности именованного канала службы (спека
/// §4.2). Дефолтный DACL named pipe даёт запись только создателю
/// (службе, SYSTEM) — не-возвышенный демон получил бы ACCESS_DENIED.
/// ACE: `GRGW` (generic read/write) для `IU` (интерактивные
/// пользователи), `FA` (полный доступ) для `SY` (SYSTEM) и `BA`
/// (администраторы).
pub const PIPE_SDDL: &str = "D:P(A;;GRGW;;;IU)(A;;FA;;;SY)(A;;FA;;;BA)";

/// Лёгкая валидация псевдонима интерфейса: непустой, не длиннее
/// [`MAX_IFACE_ALIAS_LEN`], без управляющих символов. Само имя
/// проверяет ОС при выполнении командлета — это защита в глубину
/// против мусора в канале.
pub fn validate_iface_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        Err("псевдоним интерфейса не задан".to_string())
    } else if alias.chars().count() > MAX_IFACE_ALIAS_LEN {
        Err(format!(
            "псевдоним интерфейса длиннее {MAX_IFACE_ALIAS_LEN} символов"
        ))
    } else if alias.chars().any(char::is_control) {
        Err("псевдоним интерфейса содержит управляющие символы".to_string())
    } else {
        Ok(())
    }
}

/// Диспетчер запросов демона: чистая по побочным эффектам кроме
/// операций над маршрутами ОС (только через [`route_ps_args`]) и
/// журнала. `session` — текущая сессия соединения: `Hello` выделяет
/// новую, прочие запросы требуют открытую сессию.
pub async fn dispatch(
    store: &Mutex<RouteStore>,
    session: &mut Option<SessionId>,
    req: SvcRequest,
) -> SvcReply {
    match req {
        SvcRequest::Hello { daemon_pid } => {
            // Повторный Hello на живом соединении — демон перезапустился:
            // прежняя сессия закрывается (маршруты снимаются).
            if let Some(previous) = session.take() {
                tracing::info!(
                    session = previous,
                    "повторный Hello: закрываю прежнюю сессию"
                );
                close_session_routes(store, previous).await;
            }
            let fresh = store.lock().await.next_session();
            *session = Some(fresh);
            tracing::info!(daemon_pid, session = fresh, "сессия демона открыта");
            SvcReply::Ok
        }
        SvcRequest::Add {
            dest,
            gateway,
            iface_alias,
        } => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            if let Err(error) = validate_route(dest) {
                return validation_error(&error.to_string());
            }
            // Finding 3 (ревью, спека §4.2): шлюз проверяется до
            // командлета — привилегированная поверхность службы не
            // принимает мусорные next-hop (полная проверка «достижим
            // через конкретный интерфейс» — M3).
            if let Err(error) = validate_gateway(gateway) {
                return validation_error(&error.to_string());
            }
            if let Err(message) = validate_iface_alias(&iface_alias) {
                return validation_error(&message);
            }
            // Finding 1 (ревью): идемпотентность add — зеркало Del.
            // Повторный add живого dest НЕ выходит в ОС: New-NetRoute
            // на существующем маршруте падает «объект уже существует»,
            // а host-менеджер при старте второй бутылки намеренно
            // пере-добавляет объединение чужих endpoint'ов. Идентичный
            // дубль — чистый Ok (журнал не растёт); дубль другой
            // сессии/параметров — пере-атрибуция записи текущей
            // сессией по модели журнала «последний wins» (закрытие
            // НОВОЙ сессии снимет маршрут; в ОС остаётся исходный
            // вариант маршрута — снятие идёт по dest).
            {
                let mut guard = store.lock().await;
                if let Some(existing) = guard.get(dest) {
                    if existing.session == session_id
                        && existing.gateway == gateway
                        && existing.iface_alias == iface_alias
                    {
                        tracing::info!(
                            %dest,
                            session = session_id,
                            "повторный add живого маршрута: Ok без OS-вызова"
                        );
                        return SvcReply::Ok;
                    }
                    return match guard.add(session_id, dest, gateway, iface_alias) {
                        Ok(()) => {
                            tracing::info!(
                                %dest,
                                session = session_id,
                                previous_session = existing.session,
                                "add живого маршрута пере-атрибутирован текущей сессией \
                                 (Ok без OS-вызова)"
                            );
                            SvcReply::Ok
                        }
                        Err(error) => io_error(&error.to_string()),
                    };
                }
            }
            if let Err(error) = run_ps(RouteCmd::Add, dest, Some(gateway), Some(&iface_alias)).await
            {
                return io_error(&error.to_string());
            }
            match store
                .lock()
                .await
                .add(session_id, dest, gateway, iface_alias)
            {
                Ok(()) => {
                    tracing::info!(dest = %dest, session = session_id, "маршрут добавлен");
                    SvcReply::Ok
                }
                Err(error) => io_error(&error.to_string()),
            }
        }
        SvcRequest::Del { dest } => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            if let Err(error) = validate_route(dest) {
                return validation_error(&error.to_string());
            }
            if !store.lock().await.contains(dest) {
                // Маршрута нет в live-set — работы нет (идемпотентность).
                tracing::warn!(%dest, "del маршрута, которого нет в live-set");
                return SvcReply::Ok;
            }
            if let Err(error) = run_ps(RouteCmd::Del, dest, None, None).await {
                return io_error(&error.to_string());
            }
            match store.lock().await.del(session_id, dest) {
                Ok(_) => {
                    tracing::info!(dest = %dest, session = session_id, "маршрут удалён");
                    SvcReply::Ok
                }
                Err(error) => io_error(&error.to_string()),
            }
        }
        SvcRequest::Cleanup => {
            let Some(session_id) = *session else {
                return no_session_error();
            };
            // Снимаем ВСЕ live-маршруты службы (рулинг контроллера,
            // спека §9): OS-first через общий исполнитель.
            let entries = store.lock().await.routes();
            let failures = remove_routes(store, entries).await;
            if failures.is_empty() {
                tracing::info!(session = session_id, "cleanup: live-маршруты сняты");
                SvcReply::Ok
            } else {
                io_error(&format!(
                    "не удалось удалить маршруты: {}",
                    failures.join("; ")
                ))
            }
        }
        SvcRequest::List => {
            if session.is_none() {
                return no_session_error();
            }
            SvcReply::RouteList {
                routes: store.lock().await.routes(),
            }
        }
    }
}

/// Отказ «сессия не открыта»: демон обязан начать с Hello.
fn no_session_error() -> SvcReply {
    validation_error("сессия не открыта: начните обмен с Hello")
}

/// Ответ-ошибка валидации (русский текст).
fn validation_error(message: &str) -> SvcReply {
    SvcReply::Error {
        code: svc_err::VALIDATION.to_string(),
        message: message.to_string(),
    }
}

/// Ответ-ошибка взаимодействия с ОС.
fn io_error(message: &str) -> SvcReply {
    SvcReply::Error {
        code: svc_err::IO.to_string(),
        message: message.to_string(),
    }
}

/// Закрывает сессию демона (соединение оборвалось, служба
/// останавливается либо пришёл повторный `Hello`): OS-first снятие
/// всех её маршрутов; проваленные остаются в live-set и ОС — сверка
/// при следующем старте службы удалит их как сироты (спека §9:
/// маршруты не переживают смерть демона; сверка сканирует live-set
/// журнала, а не всю таблицу ОС — опережать ОС журналом нельзя).
async fn close_session_routes(store: &Mutex<RouteStore>, session: SessionId) {
    let entries = store.lock().await.session_routes(session);
    if entries.is_empty() {
        return;
    }
    tracing::info!(
        session,
        routes = entries.len(),
        "сессия закрыта: снимаю её маршруты из ОС"
    );
    let failures = remove_routes(store, entries).await;
    if !failures.is_empty() {
        tracing::warn!(
            session,
            "часть маршрутов закрытой сессии не удалена из ОС ({}); \
             они остаются в live-set — сверка при следующем старте \
             службы удалит их как сироты",
            failures.join("; ")
        );
    }
}

/// Исполнитель удаления маршрутов по принципу OS-first: успешный
/// `Remove-NetRoute` → подтверждение [`RouteStore::del`] (запись `del`
/// в журнал + снятие из live-set); провал OS-удаления — маршрут
/// ОСТАЁТСЯ в live-set и ОС (следующая сверка удалит его как сироту).
/// Возвращает список провалов «dest: ошибка» для ответа/лога.
async fn remove_routes(store: &Mutex<RouteStore>, entries: Vec<RouteEntry>) -> Vec<String> {
    let mut failures = Vec::new();
    for entry in entries {
        match run_ps(RouteCmd::Del, entry.dest, None, None).await {
            Ok(_) => match store.lock().await.del(entry.session, entry.dest) {
                Ok(_) => tracing::info!(dest = %entry.dest, "маршрут удалён из ОС и журнала"),
                Err(error) => {
                    tracing::warn!(
                        dest = %entry.dest,
                        %error,
                        "маршрут удалён из ОС, но не записан в журнал — \
                         остаётся в live-set, сверка при старте вычистит"
                    );
                    failures.push(format!("{}: {error}", entry.dest));
                }
            },
            Err(error) => {
                tracing::warn!(
                    dest = %entry.dest,
                    %error,
                    "не удалось удалить маршрут из ОС — остаётся в live-set; \
                     сверка при следующем старте службы удалит его как сироту"
                );
                failures.push(format!("{}: {error}", entry.dest));
            }
        }
    }
    failures
}

// ----------------------------------------------------------------------
// Windows-транспорт: named pipe + LengthJsonCodec (паттерн daemon).
// ----------------------------------------------------------------------

#[cfg(windows)]
mod transport {
    use std::io;
    use std::sync::Arc;

    use bottle_core::frame::LengthJsonCodec;
    use bottle_core::svcrpc::{SvcReply, SvcRequest};
    use bytes::BytesMut;
    use tokio::io::{split, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::Mutex;
    use tokio_stream::StreamExt;
    use tokio_util::codec::{Encoder, FramedRead};
    use tokio_util::sync::CancellationToken;
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    use super::{close_session_routes, dispatch};
    use crate::journal::{RouteStore, SessionId};

    /// Обслуживает named pipe: по одному клиенту на соединение, свежий
    /// экземпляр канала на каждый цикл, до отмены `shutdown`.
    ///
    /// Канал создаётся с DACL [`super::PIPE_SDDL`] (интерактивные
    /// пользователи — чтение/запись; дефолтный DACL дал бы
    /// не-возвышенному демону ACCESS_DENIED, спека §4.2). Ошибки
    /// отдельных соединений логируются и не останавливают сервер;
    /// наружу уходит только отказ создать дескриптор безопасности
    /// или экземпляр канала.
    pub async fn serve(
        pipe_name: &str,
        store: Arc<Mutex<RouteStore>>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let mut security = PipeSecurityAttributes::create()?;
        // SAFETY: `security` живёт до конца `serve` (включая все
        // вызовы create в цикле), указатель — на валидную структуру
        // SECURITY_ATTRIBUTES; CreateNamedPipe копирует дескриптор.
        let mut server = unsafe { create_pipe_instance(pipe_name, &mut security)? };
        tracing::info!(pipe = pipe_name, "pipe-сервер службы маршрутов запущен");
        loop {
            tokio::select! {
                connected = server.connect() => connected?,
                () = shutdown.cancelled() => {
                    tracing::info!("pipe-сервер службы маршрутов остановлен по отмене");
                    return Ok(());
                }
            }
            handle_connection(server, &store, &shutdown).await;
            // SAFETY: как и выше — security ещё жив.
            server = unsafe { create_pipe_instance(pipe_name, &mut security)? };
        }
    }

    /// Создаёт экземпляр канала с дескриптором безопасности для
    /// интерактивных пользователей. Небезопасен: `security` обязан
    /// быть живым на момент вызова.
    unsafe fn create_pipe_instance(
        pipe_name: &str,
        security: &mut PipeSecurityAttributes,
    ) -> io::Result<NamedPipeServer> {
        ServerOptions::new().create_with_security_attributes_raw(pipe_name, security.as_raw())
    }

    /// Владелец SECURITY_ATTRIBUTES с дескриптором из SDDL
    /// [`super::PIPE_SDDL`]; освобождает дескриптор в Drop.
    pub(super) struct PipeSecurityAttributes {
        attributes: SECURITY_ATTRIBUTES,
    }

    impl PipeSecurityAttributes {
        /// Конвертирует SDDL-строку в дескриптор безопасности.
        pub(super) fn create() -> io::Result<Self> {
            let sddl: Vec<u16> = super::PIPE_SDDL
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // PCWSTR в windows-sys — сырой указатель на UTF-16 строку.
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                attributes: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
            })
        }

        /// Сырой указатель для `create_with_security_attributes_raw`.
        fn as_raw(&mut self) -> *mut std::ffi::c_void {
            &mut self.attributes as *mut SECURITY_ATTRIBUTES as *mut std::ffi::c_void
        }
    }

    impl Drop for PipeSecurityAttributes {
        fn drop(&mut self) {
            if !self.attributes.lpSecurityDescriptor.is_null() {
                unsafe {
                    LocalFree(self.attributes.lpSecurityDescriptor as HLOCAL);
                }
            }
        }
    }

    /// Обслуживает одно соединение демона: кадры запросов → ответы
    /// диспетчера. Завершается при отключении демона, некорректном
    /// кадре или отмене `shutdown`; в конце сессия закрывается —
    /// её маршруты убираются из ОС и журнала (демон умер).
    async fn handle_connection(
        pipe: NamedPipeServer,
        store: &Arc<Mutex<RouteStore>>,
        shutdown: &CancellationToken,
    ) {
        let mut session: Option<SessionId> = None;
        let (read_half, mut write_half) = split(pipe);
        let mut reader = FramedRead::new(read_half, LengthJsonCodec::<SvcRequest>::new());
        let mut reply_codec = LengthJsonCodec::<SvcReply>::new();

        loop {
            let req = tokio::select! {
                maybe = reader.next() => match maybe {
                    Some(Ok(req)) => req,
                    Some(Err(error)) => {
                        tracing::warn!(%error, "некорректный кадр от демона, соединение закрывается");
                        break;
                    }
                    None => {
                        tracing::info!("демон отключился");
                        break;
                    }
                },
                () = shutdown.cancelled() => break,
            };
            let reply = dispatch(store, &mut session, req).await;
            if let Err(error) = send_reply(&mut write_half, &mut reply_codec, reply).await {
                tracing::warn!(%error, "не удалось отправить ответ демону");
                break;
            }
        }

        // Соединение закрыто (демон умер/отключился либо служба
        // останавливается) — маршруты сессии не переживают (спека §9).
        if let Some(session) = session.take() {
            tracing::info!(session, "закрываю сессию демона");
            close_session_routes(store, session).await;
        }

        let pipe = reader.into_inner().unsplit(write_half);
        if let Err(error) = pipe.disconnect() {
            tracing::warn!(%error, "disconnect экземпляра канала не удался");
        }
    }

    /// Кодирует и отправляет ответ демону (fallible-путь `Encoder`).
    async fn send_reply<W>(
        write_half: &mut W,
        codec: &mut LengthJsonCodec<SvcReply>,
        reply: SvcReply,
    ) -> io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut buf = BytesMut::new();
        codec.encode(reply, &mut buf).map_err(io::Error::other)?;
        write_half.write_all(&buf).await
    }
}

#[cfg(windows)]
pub use transport::serve;

/// Не-Windows стаб транспорта: named pipe доступен только на Windows;
/// консольный режим dev-хоста ждёт сигнала остановки, чтобы журнал и
/// сверка (задача 5) оставались наблюдаемыми вне Windows.
#[cfg(not(windows))]
pub async fn serve(
    _pipe_name: &str,
    _store: std::sync::Arc<Mutex<RouteStore>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    tracing::warn!("named pipe доступен только на Windows — сервер ждёт сигнал остановки");
    shutdown.cancelled().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use bottle_core::svcrpc::SvcRequest;

    use super::*;
    use crate::journal::JOURNAL_FILE_NAME;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("тестовый адрес должен парситься")
    }

    fn add_req(dest: &str) -> SvcRequest {
        SvcRequest::Add {
            dest: ip(dest),
            gateway: ip("192.168.1.1"),
            iface_alias: "Ethernet".to_string(),
        }
    }

    /// Хранилище во временном каталоге под мьютексом (как в сервере).
    async fn test_store() -> (tempfile::TempDir, Mutex<RouteStore>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = RouteStore::open(&tmp.path().join(JOURNAL_FILE_NAME)).expect("журнал открылся");
        (tmp, Mutex::new(store))
    }

    #[test]
    fn alias_validation_rules() {
        assert_eq!(validate_iface_alias("Ethernet"), Ok(()));
        assert_eq!(validate_iface_alias("vEthernet (WSL)"), Ok(()));
        assert!(
            validate_iface_alias("").is_err(),
            "пустой алиас отвергается"
        );
        let too_long = "x".repeat(MAX_IFACE_ALIAS_LEN + 1);
        assert!(
            validate_iface_alias(&too_long).is_err(),
            "длиннее {MAX_IFACE_ALIAS_LEN} символов"
        );
        assert!(
            validate_iface_alias("Ethernet\n2").is_err(),
            "управляющие символы"
        );
        assert!(
            validate_iface_alias("Eth\x00ernet").is_err(),
            "NUL в алиасе"
        );
    }

    #[tokio::test]
    async fn requests_without_hello_are_rejected() {
        let (_tmp, store) = test_store().await;
        let mut session = None;

        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        let SvcReply::Error { code, message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(code, svc_err::VALIDATION);
        assert!(
            message.contains("Hello"),
            "сообщение подсказывает Hello: {message}"
        );
        assert!(session.is_none(), "сессия не открыта");
    }

    #[tokio::test]
    async fn add_rejects_special_dest_before_any_os_call() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        // Loopback: валидация рулит ДО вызова PowerShell — на хосте
        // разработки run_ps вернул бы NotWindows, а не VALIDATION.
        let reply = dispatch(&store, &mut session, add_req("127.0.0.1")).await;
        let SvcReply::Error { code, message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(code, svc_err::VALIDATION);
        assert!(
            message.contains("loopback"),
            "текст ошибки валидации: {message}"
        );
        assert!(
            store.lock().await.routes().is_empty(),
            "отказ валидации не трогает live-set"
        );
    }

    #[tokio::test]
    async fn add_rejects_invalid_alias() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        for alias in ["", &"y".repeat(MAX_IFACE_ALIAS_LEN + 1), "Bad\nAlias"] {
            let reply = dispatch(
                &store,
                &mut session,
                SvcRequest::Add {
                    dest: ip("203.0.113.10"),
                    gateway: ip("192.168.1.1"),
                    iface_alias: alias.to_string(),
                },
            )
            .await;
            assert!(
                matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::VALIDATION),
                "алиас {alias:?} должен отвергаться с VALIDATION, получили {reply:?}"
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn add_valid_request_reports_io_on_non_windows() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        // Валидный запрос дошёл до run_ps, но вне Windows операции над
        // маршрутами недоступны — Error IO с русским текстом.
        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        assert!(
            matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::IO),
            "ожидали Error IO, получили {reply:?}"
        );
        assert!(
            store.lock().await.routes().is_empty(),
            "провал ОС-операции не меняет live-set"
        );
    }

    /// Finding 3 (ревью): шлюз из мусорного класса адресов отвергается
    /// ДО любого OS-вызова — Error VALIDATION с русским текстом
    /// (спека §4.2: привилегированная поверхность службы).
    #[tokio::test]
    async fn add_rejects_bad_gateway_before_any_os_call() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        let reply = dispatch(
            &store,
            &mut session,
            SvcRequest::Add {
                dest: ip("203.0.113.10"),
                gateway: ip("0.0.0.0"),
                iface_alias: "Ethernet".to_string(),
            },
        )
        .await;
        let SvcReply::Error { code, message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(code, svc_err::VALIDATION);
        assert!(
            message.contains("шлюз"),
            "текст ошибки валидации шлюза: {message}"
        );
        assert!(
            store.lock().await.routes().is_empty(),
            "отказ валидации не трогает live-set"
        );
    }

    /// Finding 1 (ревью): повторный add того же dest той же сессией —
    /// Ok БЕЗ обращения к ОС (на не-Windows любой OS-вызов — Error IO,
    /// значит Ok доказывает короткое замыкание) и без новой записи в
    /// журнале — идентичный дубль ничего не меняет.
    #[tokio::test]
    async fn add_duplicate_same_session_is_ok_without_os_call() {
        let (tmp, store) = test_store().await;
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
        }
        let mut session = Some(1);

        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        assert_eq!(reply, SvcReply::Ok, "дубль add — Ok без OS-вызова");

        let journal = std::fs::read_to_string(tmp.path().join(JOURNAL_FILE_NAME)).unwrap();
        assert_eq!(
            journal.matches("\"add\"").count(),
            1,
            "идентичный дубль не пишет запись: {journal}"
        );
        assert_eq!(
            store.lock().await.routes().len(),
            1,
            "дубль не меняет live-set"
        );
    }

    /// Finding 1 (ревью): дубль add из ДРУГОЙ сессии — Ok без OS-вызова,
    /// запись пере-атрибуруется новой сессии (модель журнала «последний
    /// wins»): закрытие НОВОЙ сессии снимет маршрут, старой — нет.
    #[tokio::test]
    async fn add_duplicate_other_session_reattributes_entry() {
        let (_tmp, store) = test_store().await;
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
        }
        let mut session = Some(2);

        let reply = dispatch(&store, &mut session, add_req("203.0.113.10")).await;
        assert_eq!(reply, SvcReply::Ok, "дубль чужой сессии — Ok без OS-вызова");

        {
            let guard = store.lock().await;
            let routes = guard.routes();
            assert_eq!(routes.len(), 1, "dest один — запись одна");
            assert_eq!(routes[0].dest, ip("203.0.113.10"));
            assert_eq!(routes[0].session, 2, "владелец — новая сессия (last wins)");
            assert_eq!(
                guard.session_routes(2).len(),
                1,
                "маршрут виден закрытию НОВОЙ сессии"
            );
            assert!(
                guard.session_routes(1).is_empty(),
                "закрытие СТАРОЙ сессии маршрут не снимет"
            );
        }
        // Подтверждение снятия приходит от новой сессии, не от старой.
        assert!(
            store.lock().await.del(1, ip("203.0.113.10")).unwrap(),
            "del по живому dest снимает маршрут (сессия в записи — новая)"
        );
    }

    #[tokio::test]
    async fn del_of_untracked_dest_is_idempotent_ok() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        let reply = dispatch(
            &store,
            &mut session,
            SvcRequest::Del {
                dest: ip("203.0.113.99"),
            },
        )
        .await;
        assert_eq!(reply, SvcReply::Ok, "нет маршрута — нет работы");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn del_tracked_dest_reports_io_failure_and_keeps_state() {
        let (_tmp, store) = test_store().await;
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
        }
        let mut session = Some(1);

        let reply = dispatch(
            &store,
            &mut session,
            SvcRequest::Del {
                dest: ip("203.0.113.10"),
            },
        )
        .await;
        assert!(
            matches!(reply, SvcReply::Error { ref code, .. } if code == svc_err::IO),
            "ожидали Error IO вне Windows, получили {reply:?}"
        );
        assert_eq!(
            store.lock().await.routes().len(),
            1,
            "провал удаления из ОС оставляет маршрут в live-set"
        );
    }

    #[tokio::test]
    async fn hello_opens_session_and_rehello_closes_previous() {
        let (_tmp, store) = test_store().await;
        let mut session = None;

        // Первый Hello: прежних живых сессий нет, открывается новая.
        let reply = dispatch(&store, &mut session, SvcRequest::Hello { daemon_pid: 4242 }).await;
        assert_eq!(reply, SvcReply::Ok);
        let first = session.expect("сессия открыта");

        // Повторный Hello на живом соединении — перезапуск сессии:
        // прежняя закрывается по принципу OS-first (см. тест ниже),
        // открывается новая с монотонно большим номером.
        store
            .lock()
            .await
            .add(first, ip("203.0.113.11"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        let reply = dispatch(&store, &mut session, SvcRequest::Hello { daemon_pid: 4242 }).await;
        assert_eq!(reply, SvcReply::Ok);
        let second = session.expect("новая сессия открыта");
        assert_ne!(first, second, "сессии монотонно нумеруются");
        assert_eq!(session, Some(second), "соединение привязано к новой сессии");
    }

    /// Finding 1 (ревью): провал OS-удаления при закрытии сессии НЕ
    /// должен вычищать маршрут из live-set и журнала — иначе маршрут
    /// теряется из-под контроля навсегда (сверка сканирует live-set
    /// журнала, а не всю таблицу ОС).
    ///
    /// ОС-удаление здесь детерминированно проваливается: вне Windows
    /// run_ps всегда NotWindows; на Windows тестовых маршрутов в
    /// ActiveStore нет — Remove-NetRoute с -ErrorAction Stop падает.
    #[tokio::test]
    async fn session_close_keeps_routes_when_os_del_fails() {
        let (tmp, store) = test_store().await;
        {
            let mut guard = store.lock().await;
            guard
                .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
            guard
                .add(1, ip("203.0.113.11"), ip("192.168.1.1"), "Ethernet")
                .unwrap();
            guard
                .add(2, ip("203.0.113.12"), ip("192.168.1.1"), "Wi-Fi")
                .unwrap();
        }

        close_session_routes(&store, 1).await;

        {
            let guard = store.lock().await;
            assert_eq!(
                guard.routes().len(),
                3,
                "провал OS-удаления: маршруты остаются в live-set"
            );
            assert_eq!(
                guard.session_routes(1).len(),
                2,
                "маршруты мёртвой сессии остаются под контролем сверки"
            );
        }
        let journal = std::fs::read_to_string(tmp.path().join(JOURNAL_FILE_NAME)).unwrap();
        assert_eq!(
            journal.matches("\"del\"").count(),
            0,
            "провал OS-удаления не пишет del в журнал: {journal}"
        );
        assert_eq!(journal.matches("\"add\"").count(), 3);
    }

    #[tokio::test]
    async fn list_returns_live_set_snapshot() {
        let (_tmp, store) = test_store().await;
        store
            .lock()
            .await
            .add(1, ip("203.0.113.10"), ip("192.168.1.1"), "Ethernet")
            .unwrap();
        let mut session = Some(1);

        let reply = dispatch(&store, &mut session, SvcRequest::List).await;
        let SvcReply::RouteList { routes } = reply else {
            panic!("ожидали RouteList, получили {reply:?}");
        };
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].dest, ip("203.0.113.10"));
        assert_eq!(routes[0].session, 1);
    }

    #[tokio::test]
    async fn cleanup_on_empty_live_set_is_ok() {
        let (_tmp, store) = test_store().await;
        let mut session = Some(1);

        let reply = dispatch(&store, &mut session, SvcRequest::Cleanup).await;
        assert_eq!(reply, SvcReply::Ok, "чистый live-set — немедленный успех");
    }

    /// Пин DACL именованного канала: интерактивные пользователи —
    /// чтение/запись (демон без повышения прав, спека §4.2), SYSTEM и
    /// администраторы — полный доступ. Изменение состава ACE —
    /// осознанное решение безопасности, а не случайный рефакторинг.
    #[test]
    fn pipe_sddl_pins_interactive_rw_and_admins_full() {
        assert!(
            PIPE_SDDL.starts_with("D:P("),
            "DACL без наследования: {PIPE_SDDL}"
        );
        assert!(
            PIPE_SDDL.contains("(A;;GRGW;;;IU)"),
            "интерактивным пользователям — generic read/write: {PIPE_SDDL}"
        );
        assert!(
            PIPE_SDDL.contains("(A;;FA;;;SY)") && PIPE_SDDL.contains("(A;;FA;;;BA)"),
            "SYSTEM и администраторы — полный доступ: {PIPE_SDDL}"
        );
    }

    /// SDDL-строка обязана конвертироваться в реальный дескриптор
    /// безопасности (запускается на Windows-CI в обычном прогоне
    /// тестов; прав администратора не требует).
    #[cfg(windows)]
    #[test]
    fn pipe_sddl_converts_to_security_descriptor() {
        transport::PipeSecurityAttributes::create()
            .expect("SDDL канала конвертируется в SECURITY_ATTRIBUTES");
    }
}
