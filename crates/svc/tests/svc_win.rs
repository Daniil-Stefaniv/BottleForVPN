//! Поведенческий тест службы маршрутов целиком (только Windows, под
//! администратором): установка реальной службы → RPC по named pipe
//! (Hello → Add → List) → обрыв клиентского соединения → маршруты
//! сессии исчезают из ОС ≤10 с (правило сессии, спека §9) → повторное
//! подключение → Cleanup → остановка службы → «переживший» её маршрут
//! + запись в журнале → повторный запуск сверки подбирает сироту.
//!
//! Запуск: `cargo test -p svc --test svc_win -- --ignored`
//! (джоба CI `windows-checks`: раннеры под администратором; адреса из
//! RFC 5737 TEST-NET-3 гарантированно ничьи).

#![cfg(windows)]

use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use bottle_core::frame::{encode_frame, LengthJsonCodec};
use bottle_core::svcrpc::{SvcReply, SvcRequest, SVC_PIPE};
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio_util::codec::Decoder;

/// Имя службы — контракт реестра SCM (main.rs).
const SERVICE_NAME: &str = "PogrebokRouteHelper";

/// Маршрут фазы «живой сессии» (RFC 5737 TEST-NET-3).
const SESSION_DEST: &str = "203.0.113.78";

/// Маршрут фазы «сирота, переживший смерть службы» (RFC 5737).
const STALE_DEST: &str = "203.0.113.79";

/// Мёртвая сессия для сидированной записи журнала (фаза сверки).
const DEAD_SESSION: u64 = 999;

/// Выполняет PowerShell-скрипт и возвращает stdout.
fn ps(script: &str) -> String {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell.exe доступен на Windows-раннере");
    assert!(
        output.status.success(),
        "PowerShell-скрипт завершился с ошибкой: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Путь журнала службы (тот же контракт, что в journal.rs).
fn journal_path() -> PathBuf {
    let programdata =
        std::env::var_os("PROGRAMDATA").expect("PROGRAMDATA задан на Windows-раннере");
    std::path::Path::new(&programdata)
        .join("Pogrebok")
        .join("routes.jsonl")
}

/// Шлюз и интерфейс маршрута по умолчанию хоста (0.0.0.0/0, маршрут с
/// наименьшей метрикой): тест добавляет точечные маршруты рядом с
/// магистральным, не выдумывая топологию.
fn default_route() -> (IpAddr, String) {
    let script =
        "(Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Sort-Object RouteMetric | Select-Object -First 1)";
    let gateway = ps(&format!("{script}.NextHop")).trim().to_string();
    let alias = ps(&format!("{script}.InterfaceAlias")).trim().to_string();
    assert!(
        !gateway.is_empty() && !alias.is_empty(),
        "на хосте нет маршрута по умолчанию (0.0.0.0/0) — тест не может выбрать шлюз"
    );
    (
        gateway
            .parse()
            .expect("NextHop маршрута по умолчанию — IPv4-адрес"),
        alias,
    )
}

/// Есть ли точечный маршрут /32 в таблице маршрутов ОС.
fn route_exists(dest: &str) -> bool {
    let script = format!(
        "if (Get-NetRoute -DestinationPrefix '{dest}/32' -ErrorAction SilentlyContinue) {{ 'present' }} else {{ 'absent' }}"
    );
    ps(&script).trim().contains("present")
}

/// Добавляет точечный маршрут напрямую через PowerShell — имитация
/// маршрута, пережившего смерть службы (для фазы сверки).
fn add_route_direct(dest: &str, gateway: IpAddr, alias: &str) {
    ps(&format!(
        "New-NetRoute -DestinationPrefix '{dest}/32' -NextHop '{gateway}' \
         -InterfaceAlias '{alias}' -PolicyStore ActiveStore -ErrorAction Stop"
    ));
}

/// Best-effort удаление точечного маршрута (уборка, ошибки молча).
fn del_route_quiet(dest: &str) {
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Remove-NetRoute -DestinationPrefix '{dest}/32' -PolicyStore ActiveStore \
                 -Confirm:$false -ErrorAction SilentlyContinue"
            ),
        ])
        .output();
}

/// Выполняет `sc.exe` и возвращает stdout (для статусов службы).
fn sc_out(args: &[&str]) -> String {
    let output = Command::new("sc.exe")
        .args(args)
        .output()
        .expect("sc.exe доступен");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Ждёт (блокирующе, для teardown) состояние службы в SCM.
fn wait_service_state_sync(state: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if sc_out(&["query", SERVICE_NAME]).contains(state) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Ждёт (async) состояние службы в SCM.
async fn wait_service_state(state: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if sc_out(&["query", SERVICE_NAME]).contains(state) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("служба не достигла состояния {state} за {timeout:?}");
}

/// Подключается к каналу службы с повторами: между соединениями (и до
/// первого) экземпляра канала может не быть — это ожидаемо.
async fn wait_pipe() -> NamedPipeClient {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ClientOptions::new().open(SVC_PIPE) {
            Ok(client) => return client,
            Err(error)
                if matches!(error.raw_os_error(),
                    Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "канал службы {SVC_PIPE} не появился за 30 с"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("не удалось подключиться к каналу {SVC_PIPE}: {error}"),
        }
    }
}

/// Обмен «запрос → ответ» через клиентский конец канала.
async fn exchange(client: &mut NamedPipeClient, req: SvcRequest) -> SvcReply {
    client
        .write_all(&encode_frame(&req))
        .await
        .expect("запрос отправлен");

    let mut codec = LengthJsonCodec::<SvcReply>::new();
    let mut buf = BytesMut::new();
    loop {
        if let Some(reply) = codec.decode(&mut buf).expect("ответ разбирается") {
            return reply;
        }
        let mut chunk = [0u8; 4096];
        let n = client.read(&mut chunk).await.expect("чтение ответа");
        assert!(n > 0, "служба закрыла канал до ответа");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Ждёт исчезновения маршрута из ОС (правило сессии: ≤10 с).
async fn wait_route_absent(dest: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !route_exists(dest) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("маршрут {dest} не исчез за {timeout:?} — правило сессии нарушено");
}

/// Дописывает в журнал службы запись add «мёртвой» сессии — сидирование
/// фазы сверки (схема строки — контракт journal.rs).
fn append_journal_add(dest: &str, gateway: IpAddr, alias: &str) {
    let line = serde_json::json!({
        "op": "add",
        "dest": dest,
        "gateway": gateway.to_string(),
        "iface_alias": alias,
        "session": DEAD_SESSION,
        "ts": 0,
    })
    .to_string();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path())
        .expect("журнал службы открыт для дописи");
    writeln!(file, "{line}").expect("допись записи в журнал");
}

/// Пожалуйные работы: остановка и удаление службы, уборка тестовых
/// маршрутов и журнала — гарантированно выполняются и при провале
/// теста (Drop), чтобы прогон не оставлял следов в системе.
struct TestService {
    installed: bool,
}

impl TestService {
    /// Устанавливает службу собственным бинарником (`--install`).
    fn install() -> Self {
        let output = Command::new(env!("CARGO_BIN_EXE_pogrebok-svc"))
            .arg("--install")
            .output()
            .expect("запуск pogrebok-svc --install");
        assert!(
            output.status.success(),
            "установка службы не удалась: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Self { installed: true }
    }
}

impl Drop for TestService {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        let _ = Command::new("sc.exe").args(["stop", SERVICE_NAME]).output();
        wait_service_state_sync("STOPPED", Duration::from_secs(30));
        let _ = Command::new(env!("CARGO_BIN_EXE_pogrebok-svc"))
            .arg("--uninstall")
            .output();
        del_route_quiet(SESSION_DEST);
        del_route_quiet(STALE_DEST);
        let _ = std::fs::remove_file(journal_path());
        self.installed = false;
    }
}

#[tokio::test]
#[ignore = "ставит/удаляет реальную службу и меняет таблицу маршрутов; запускать под администратором"]
async fn service_sessions_and_reconcile_end_to_end() {
    let guard = TestService::install();

    // Остатки предыдущих прогонов (недобитый teardown) не должны
    // маскировать результат: чистим журнал и тестовые маршруты.
    let _ = std::fs::remove_file(journal_path());
    del_route_quiet(SESSION_DEST);
    del_route_quiet(STALE_DEST);

    let (gateway, alias) = default_route();

    // --- Фаза A: живая сессия демона. ------------------------------
    sc_out(&["start", SERVICE_NAME]);
    wait_service_state("RUNNING", Duration::from_secs(30)).await;

    let mut client = wait_pipe().await;
    assert_eq!(
        exchange(
            &mut client,
            SvcRequest::Hello {
                daemon_pid: std::process::id()
            }
        )
        .await,
        SvcReply::Ok
    );
    assert_eq!(
        exchange(
            &mut client,
            SvcRequest::Add {
                dest: SESSION_DEST.parse().unwrap(),
                gateway,
                iface_alias: alias.clone(),
            }
        )
        .await,
        SvcReply::Ok,
        "New-NetRoute через службу должен проходить под администратором"
    );

    let reply = exchange(&mut client, SvcRequest::List).await;
    let SvcReply::RouteList { routes } = reply else {
        panic!("ожидали RouteList, получили {reply:?}");
    };
    let entry = routes
        .iter()
        .find(|entry| entry.dest.to_string() == SESSION_DEST)
        .expect("маршрут сессии виден в списке службы");
    assert!(entry.session != 0, "маршрут привязан к сессии: {entry:?}");
    assert!(route_exists(SESSION_DEST), "маршрут появился в таблице ОС");

    // --- Фаза B: смерть демона = обрыв клиентского соединения. -----
    drop(client);
    // Правило сессии (спека §9): маршруты исчезают ≤10 с.
    wait_route_absent(SESSION_DEST, Duration::from_secs(10)).await;

    // --- Фаза C: новая сессия, cleanup с непустым live-set. ---------
    let mut client = wait_pipe().await;
    assert_eq!(
        exchange(
            &mut client,
            SvcRequest::Hello {
                daemon_pid: std::process::id()
            }
        )
        .await,
        SvcReply::Ok
    );
    let reply = exchange(&mut client, SvcRequest::List).await;
    let SvcReply::RouteList { routes } = reply else {
        panic!("ожидали RouteList, получили {reply:?}");
    };
    assert!(
        routes.is_empty(),
        "после смерти сессии live-set пуст: {routes:?}"
    );

    // Наполняем live-set заново: Cleanup обязан пройти РЕАЛЬНУЮ ветку
    // удаления (Remove-NetRoute по каждому маршруту), а не пустой цикл.
    assert_eq!(
        exchange(
            &mut client,
            SvcRequest::Add {
                dest: SESSION_DEST.parse().unwrap(),
                gateway,
                iface_alias: alias.clone(),
            }
        )
        .await,
        SvcReply::Ok
    );
    assert!(route_exists(SESSION_DEST), "маршрут снова в таблице ОС");
    assert_eq!(
        exchange(&mut client, SvcRequest::Cleanup).await,
        SvcReply::Ok,
        "cleanup снимает live-маршруты через Remove-NetRoute"
    );
    wait_route_absent(SESSION_DEST, Duration::from_secs(10)).await;
    let reply = exchange(&mut client, SvcRequest::List).await;
    let SvcReply::RouteList { routes } = reply else {
        panic!("ожидали RouteList, получили {reply:?}");
    };
    assert!(routes.is_empty(), "после cleanup live-set пуст: {routes:?}");
    drop(client);

    // --- Фаза D: сверка журнала при повторном старте службы. -------
    // Останавливаем службу, «воскрешаем» маршрут напрямую и дописываем
    // запись add мёртвой сессии в журнал — служба при старте обязана
    // распознать сироту и удалить его из ОС.
    let _ = Command::new("sc.exe").args(["stop", SERVICE_NAME]).output();
    wait_service_state("STOPPED", Duration::from_secs(30)).await;

    add_route_direct(STALE_DEST, gateway, &alias);
    append_journal_add(STALE_DEST, gateway, &alias);

    sc_out(&["start", SERVICE_NAME]);
    wait_service_state("RUNNING", Duration::from_secs(30)).await;
    // Канал появляется после завершения сверки (reconcile идёт до
    // pipe-сервера): подключение = сигнал «сверка закончена».
    let mut client = wait_pipe().await;

    wait_route_absent(STALE_DEST, Duration::from_secs(15)).await;
    assert_eq!(
        exchange(
            &mut client,
            SvcRequest::Hello {
                daemon_pid: std::process::id()
            }
        )
        .await,
        SvcReply::Ok
    );
    let reply = exchange(&mut client, SvcRequest::List).await;
    let SvcReply::RouteList { routes } = reply else {
        panic!("ожидали RouteList, получили {reply:?}");
    };
    assert!(
        routes.is_empty(),
        "сирота вычищен из live-set сверкой: {routes:?}"
    );
    drop(client);

    drop(guard);
}
