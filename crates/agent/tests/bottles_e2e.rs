//! Интеграционный тест полного Up/Down API `Bottles` (задача 11).
//!
//! Работает только на Linux под root (CI-джоба `agent-integration`).
//! Стенд повторяет wg_handshake (задача 10) — «VPN-сервер» в отдельном
//! namespace, но бутылка поднимается целиком через `Bottles::up`:
//!
//! ```text
//! [default ns: хост]            [ns t-be2e-srv]            [ns bottle-t-e2e]
//!  p<idx> 10.77.idx.1 ←veth→    t-be2e-s 10.77.253.1/30 ←veth→ b<idx> 10.77.idx.2
//!  t-be2e-h 10.77.253.2/30 ←────┘     wgsrv 10.99.0.1        wg<idx> 10.99.0.2
//!  мосты 0.0.0.0:49210/:49211          echo на 10.99.0.1      relay + туннель
//! ```
//!
//! Проверяется контракт `Bottles`: `up` → `Ready`; tmpfs-каталог
//! `/run/bottles/<id>/` с конфигом и pid-файлами; `Status(Some)` →
//! state "up"; повторный `up` того же id → `ALREADY_UP`; сквозные
//! SOCKS5 и TCP-проброс через мосты до echo; `down` чужого
//! instance-поколения → `STALE_INSTANCE` и бутылка жива (протокол v2);
//! `down` своего поколения → `Ok` и полное освобождение (namespace,
//! порты, слоты); `up` другого id после `down` работает. Запуск:
//!
//! ```sh
//! sudo -E cargo test -p agent --test bottles_e2e -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::future::Future;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agent::bottles::Bottles;
use bottle_core::profile::TunnelProtocol;
use bottle_core::protocol::{err, AgentReply, BottleStatusInfo, ForwardPort, UpArgs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Идентификатор первой тестовой бутылки (namespace `bottle-t-e2e`).
const ID: &str = "t-e2e";
/// Идентификатор второй бутылки (проверка освобождения слотов/портов).
const ID2: &str = "t-e2e2";
/// Namespace «VPN-сервера». Имя без префикса `bottle-`, чтобы остатки
/// аварийного прогона не ломали ассерты тестов netns_lifecycle.
const NS_SRV: &str = "t-be2e-srv";
/// Хостовый конец veth до srv; второй конец живёт в srv.
const VETH_HOST: &str = "t-be2e-h";
/// Конец veth внутри srv.
const VETH_SRV: &str = "t-be2e-s";
/// Адрес srv-конца veth — endpoint туннеля (подсеть 253 вне слотов 1..=250).
const SRV_VETH_ADDR: Ipv4Addr = Ipv4Addr::new(10, 77, 253, 1);
/// Адрес хостового конца veth до srv.
const HOST_VETH_ADDR: Ipv4Addr = Ipv4Addr::new(10, 77, 253, 2);
/// Внутренний адрес wg-интерфейса сервера (на нём слушает echo).
const WG_SRV_ADDR: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
/// Внутренний адрес wg-интерфейса клиента (бутылки).
const WG_CLI_ADDR: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 2);
/// ListenPort сервера WireGuard.
const WG_PORT: u16 = 51820;
/// Порт SOCKS5-моста бутылки в default namespace (продолжение нумерации:
/// 49200 netns_lifecycle, 49201 wg_handshake).
const SOCKS5_PORT: u16 = 49210;
/// Порт TCP-проброса бутылки в default namespace.
const FORWARD_PORT: u16 = 49211;
/// Instance-поколение демона в этом стенде (протокол v2).
const INSTANCE: u64 = 7;
/// Общий предел ожидания асинхронных событий (relay, echo).
const WAIT: Duration = Duration::from_secs(15);
/// Пауза между попытками.
const RETRY: Duration = Duration::from_millis(100);
/// Предел ожидания готовности wireguard-go в srv.
const WG_READY_WAIT: Duration = Duration::from_secs(5);
/// Пауза между опросами готовности.
const WG_READY_POLL: Duration = Duration::from_millis(50);

/// Полный жизненный цикл через публичное API `Bottles`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "нужны root, iproute2 и wireguard (CI-джоба agent-integration)"]
async fn bottles_up_down_full_cycle() {
    assert_root();
    // Relay внутри бутылки — собранный bottle-agent (шов задачи 9).
    std::env::set_var("BOTTLE_AGENT_RELAY_EXE", env!("CARGO_BIN_EXE_bottle-agent"));
    cleanup_leftovers();

    // Хост маршрутизирует бутылку ↔ srv: оба veth кончаются в default ns.
    sysctl("net.ipv4.ip_forward", "1");
    // Docker на GitHub-раннерах держит политику FORWARD=DROP.
    allow_test_forward();
    // rp_filter ослабляем до создания интерфейсов бутылки: veth-появляется
    // после `default=0` и наследует его (номер слота снаружи не виден).
    relax_rp_filter();

    // (1) srv-namespace + veth 10.77.253.0/30 (вручную: 253 вне слотов).
    run_ok("ip", &["netns", "add", NS_SRV]);
    run_ok(
        "ip",
        &[
            "link", "add", VETH_HOST, "type", "veth", "peer", "name", VETH_SRV,
        ],
    );
    run_ok("ip", &["link", "set", VETH_SRV, "netns", NS_SRV]);
    run_ok(
        "ip",
        &[
            "addr",
            "add",
            &format!("{HOST_VETH_ADDR}/30"),
            "dev",
            VETH_HOST,
        ],
    );
    run_ok("ip", &["link", "set", VETH_HOST, "up"]);
    run_ok(
        "ip",
        &[
            "netns",
            "exec",
            NS_SRV,
            "ip",
            "addr",
            "add",
            &format!("{SRV_VETH_ADDR}/30"),
            "dev",
            VETH_SRV,
        ],
    );
    run_ok(
        "ip",
        &["netns", "exec", NS_SRV, "ip", "link", "set", VETH_SRV, "up"],
    );
    run_ok(
        "ip",
        &["netns", "exec", NS_SRV, "ip", "link", "set", "lo", "up"],
    );
    run_ok(
        "ip",
        &[
            "netns",
            "exec",
            NS_SRV,
            "ip",
            "route",
            "add",
            "default",
            "via",
            &HOST_VETH_ADDR.to_string(),
        ],
    );

    // (2) Пары ключей — только генерацией wg genkey/wg pubkey на стенде.
    let srv_priv = wg_genkey();
    let srv_pub = wg_pubkey(&srv_priv);
    let cli_priv = wg_genkey();
    let cli_pub = wg_pubkey(&cli_priv);

    // (3) Сервер WireGuard в srv: имя wgsrv не пересекается с бутылочными
    // wg<idx> — UAPI-сокеты живут в общей файловой системе.
    // WG_PROCESS_FOREGROUND=1: без неё wireguard-go демонизируется, и
    // wait_wg_ready видит «смерть» родителя («умер при старте»).
    let mut srv_wg = Command::new("ip")
        .args(["netns", "exec", NS_SRV, "wireguard-go", "wgsrv"])
        .env("WG_PROCESS_FOREGROUND", "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("запустить wireguard-go в srv");
    wait_wg_ready(NS_SRV, "wgsrv", &mut srv_wg);
    let srv_conf = format!(
        "[Interface]\n\
         PrivateKey = {srv_priv}\n\
         ListenPort = {WG_PORT}\n\
         \n\
         [Peer]\n\
         PublicKey = {cli_pub}\n\
         AllowedIPs = {WG_CLI_ADDR}/32\n"
    );
    run_stdin_ok(
        &[
            "netns",
            "exec",
            NS_SRV,
            "wg",
            "setconf",
            "wgsrv",
            "/dev/stdin",
        ],
        &srv_conf,
    );
    run_ok(
        "ip",
        &["netns", "exec", NS_SRV, "ip", "link", "set", "wgsrv", "up"],
    );
    run_ok(
        "ip",
        &[
            "netns",
            "exec",
            NS_SRV,
            "ip",
            "addr",
            "add",
            &format!("{WG_SRV_ADDR}/32"),
            "dev",
            "wgsrv",
        ],
    );
    // Ответы echo клиенту уходят через wgsrv (AllowedIPs не создают маршруты).
    run_ok(
        "ip",
        &[
            "netns",
            "exec",
            NS_SRV,
            "ip",
            "route",
            "add",
            &format!("{WG_CLI_ADDR}/32"),
            "dev",
            "wgsrv",
        ],
    );

    // (4) Echo на 10.99.0.1 внутри srv: поток входит в namespace (setns)
    // и биндит сокет; акцепт-цикл живёт в tokio-рантайме теста.
    let echo_addr = spawn_echo_in_ns(NS_SRV, WG_SRV_ADDR).expect("поднять echo в srv");

    // (5) Up через публичное API: WireGuard, SOCKS5-мост и TCP-проброс.
    let bottles = Bottles::new();
    let args = UpArgs {
        id: ID.to_string(),
        protocol: TunnelProtocol::WireGuard,
        config: client_config(&srv_pub, &cli_priv),
        dns: None,
        socks5_port: SOCKS5_PORT,
        forwards: vec![ForwardPort {
            port: FORWARD_PORT,
            target: format!("{WG_SRV_ADDR}:{}", echo_addr.port()),
        }],
        instance: INSTANCE,
        username: None,
        password: None,
    };

    let reply = bottles.up(args.clone()).await;
    assert_eq!(
        reply,
        AgentReply::Ready { id: ID.to_string() },
        "up → Ready"
    );

    // tmpfs-каталог бутылки: конфиг и pid-файлы relay/туннеля.
    let run_dir = Path::new("/run/bottles").join(ID);
    assert!(
        run_dir.join("wg.conf").exists(),
        "нет конфига в {run_dir:?}"
    );
    assert!(
        run_dir.join("pid-relay").exists(),
        "нет pid-файла relay в {run_dir:?}"
    );
    assert!(
        run_dir.join("pid-tunnel").exists(),
        "нет pid-файла туннеля в {run_dir:?}"
    );

    // (6) Status одной бутылки: state "up".
    assert_eq!(
        bottles.status(Some(ID)),
        vec![BottleStatusInfo {
            id: ID.to_string(),
            state: "up".to_string(),
            detail: None,
        }],
        "Status(Some) должен показывать живую бутылку"
    );
    assert_eq!(bottles.status(Some("ghost")), vec![], "чужой id → пусто");

    // (7) Повторный up того же id → ALREADY_UP (Review Focus #4).
    let reply = bottles.up(args.clone()).await;
    assert_eq!(error_code(reply), err::ALREADY_UP, "живой id → ALREADY_UP");

    // (8) Сквозной SOCKS5 через мост 49210 → relay → wg<idx> → echo.
    socks5_connect_echo_with_retry(SOCKS5_PORT, echo_addr).await;

    // (9) Сквозной TCP-проброс: мост 49211 → relay → туннель → echo.
    forward_connect_echo_with_retry(FORWARD_PORT).await;

    // (10) Down чужого поколения → STALE_INSTANCE, бутылка жива
    // (протокол v2: TOCTOU-защита от Down проигравшего рестарта).
    let reply = bottles.down(ID, INSTANCE + 1).await;
    assert_eq!(
        error_code(reply),
        err::STALE_INSTANCE,
        "чужое поколение → STALE_INSTANCE"
    );
    assert_eq!(
        bottles.status(Some(ID)).len(),
        1,
        "бутылка жива после чужого Down"
    );

    // (11) Down своего поколения → Ok; namespace и tmpfs-каталог
    // разобраны полностью.
    let reply = bottles.down(ID, INSTANCE).await;
    assert_eq!(reply, AgentReply::Ok, "down → Ok");
    let list = ip_netns_list();
    assert!(
        !list.contains("bottle-"),
        "после down в ip netns list не должно быть bottle-*: {list}"
    );
    assert!(
        !run_dir.exists(),
        "tmpfs-каталог {run_dir:?} не удалён после down"
    );

    // (12) Слоты и порты свободны: up другого id на тех же портах работает.
    let args2 = UpArgs {
        id: ID2.to_string(),
        ..args
    };
    let reply = bottles.up(args2).await;
    assert_eq!(
        reply,
        AgentReply::Ready {
            id: ID2.to_string()
        },
        "up после down → Ready"
    );
    // Мосты переиспользуют освобождённые порты: сквозной путь жив.
    socks5_connect_echo_with_retry(SOCKS5_PORT, echo_addr).await;
    let reply = bottles.down(ID2, INSTANCE).await;
    assert_eq!(reply, AgentReply::Ok, "down второй бутылки → Ok");

    // (13) Разбор стенда.
    kill_wait(&mut srv_wg);
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
}

/// Клиентский конфиг WireGuard: endpoint — адрес srv-конца veth
/// (dotted-IPv4 — резолв замыкается без DNS), туннельный адрес 10.99.0.2.
///
/// `cli_priv` — приватный ключ из шага генерации пар: сервер зарегистри-
/// ровал производный от него публичный ключ как свой пир, другой ключ
/// не пройдёт handshake.
fn client_config(srv_pub: &str, cli_priv: &str) -> String {
    format!(
        "[Interface]\n\
         Address = {WG_CLI_ADDR}/32\n\
         PrivateKey = {cli_priv}\n\
         \n\
         [Peer]\n\
         PublicKey = {srv_pub}\n\
         Endpoint = {SRV_VETH_ADDR}:{WG_PORT}\n\
         AllowedIPs = {WG_SRV_ADDR}/32\n\
         PersistentKeepalive = 5\n"
    )
}

/// Код ошибки из ответа (паникует, если ответ не Error).
fn error_code(reply: AgentReply) -> String {
    match reply {
        AgentReply::Error { code, .. } => code,
        other => panic!("ожидалась ошибка, получено {other:?}"),
    }
}

/// Паникует с внятным сообщением, если тест запущен не под root.
fn assert_root() {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .expect("запустить id")
        .stdout;
    assert_eq!(
        String::from_utf8_lossy(&uid).trim(),
        "0",
        "тест требует root: sudo -E cargo test -p agent --test bottles_e2e -- --ignored"
    );
}

/// Убирает остатки прошлых прогонов: namespaces, veth, каталоги DNS и tmpfs.
fn cleanup_leftovers() {
    for id in [ID, ID2] {
        let _ = Command::new("ip")
            .args(["netns", "del", &format!("bottle-{id}")])
            .status();
        let _ = std::fs::remove_dir_all(format!("/etc/netns/bottle-{id}"));
        let _ = std::fs::remove_dir_all(format!("/run/bottles/{id}"));
    }
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
    // SIGKILL'нутый wireguard-go оставляет UAPI-сокет: следующий стенд
    // с тем же именем интерфейса не должен его наследовать.
    let _ = std::fs::remove_file("/var/run/wireguard/wgsrv.sock");
}

/// Возвращает stdout `ip netns list`.
fn ip_netns_list() -> String {
    String::from_utf8_lossy(
        &Command::new("ip")
            .args(["netns", "list"])
            .output()
            .expect("запустить ip netns list")
            .stdout,
    )
    .into_owned()
}

/// Выполняет команду, паникуя с stderr при неудаче.
fn run_ok(program: &str, args: &[&str]) {
    let Output { status, stderr, .. } = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("запустить {program} {}: {error}", args.join(" ")));
    assert!(
        status.success(),
        "{program} {} не удалась: {}",
        args.join(" "),
        String::from_utf8_lossy(&stderr).trim()
    );
}

/// Выполняет `sysctl -w key=value`.
fn sysctl(key: &str, value: &str) {
    run_ok("sysctl", &["-w", &format!("{key}={value}")]);
}

/// Точечное ACCEPT-правило FORWARD для стендовых подсетей: на
/// GitHub-раннерах предустановлен Docker, его политика FORWARD=DROP
/// роняет маршрутизируемый хостом трафик бутылка ↔ srv. Идемпотентно:
/// `-C` проверяет наличие, `-I` вставляет.
fn allow_test_forward() {
    let rule = [
        "FORWARD",
        "-s",
        "10.77.0.0/16",
        "-d",
        "10.77.0.0/16",
        "-j",
        "ACCEPT",
    ];
    let present = Command::new("iptables")
        .arg("-C")
        .args(rule)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !present {
        let _ = Command::new("iptables").arg("-I").args(rule).status();
    }
}

/// Ослабляет rp_filter: all + default. Путь бутылка→srv→echo асимметричен
/// с точки зрения интерфейса, strict rp_filter роняет его; `default`
/// закрывает veth-интерфейсы бутылки, создаваемые после вызова.
fn relax_rp_filter() {
    let _ = Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.all.rp_filter=0"])
        .status();
    let _ = Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.default.rp_filter=0"])
        .status();
}

/// Генерирует приватный ключ WireGuard (`wg genkey`).
fn wg_genkey() -> String {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new("wg")
        .arg("genkey")
        .output()
        .expect("запустить wg genkey");
    assert!(
        status.success(),
        "wg genkey не удался: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    String::from_utf8(stdout)
        .expect("ключ — UTF-8")
        .trim()
        .to_string()
}

/// Выводит публичный ключ для приватного (`wg pubkey`, ключ в stdin).
fn wg_pubkey(private: &str) -> String {
    let mut child = Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("запустить wg pubkey");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(private.as_bytes())
        .expect("передать приватный ключ в wg pubkey");
    let Output {
        status,
        stdout,
        stderr,
    } = child.wait_with_output().expect("wg pubkey output");
    assert!(
        status.success(),
        "wg pubkey не удался: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    String::from_utf8(stdout)
        .expect("ключ — UTF-8")
        .trim()
        .to_string()
}

/// Ждёт готовности wireguard-go (интерфейс + UAPI-сокет): `wg show`
/// отвечает только когда есть оба. Ранний выход при смерти процесса.
fn wait_wg_ready(ns: &str, iface: &str, child: &mut Child) {
    let deadline = Instant::now() + WG_READY_WAIT;
    loop {
        let ready = Command::new("ip")
            .args(["netns", "exec", ns, "wg", "show", iface])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if ready {
            return;
        }
        let dead = child
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(true);
        assert!(!dead, "wireguard-go в {ns} умер при старте");
        assert!(
            Instant::now() < deadline,
            "интерфейс {iface} не готов за {WG_READY_WAIT:?} в {ns}"
        );
        std::thread::sleep(WG_READY_POLL);
    }
}

/// Выполняет `ip <args>`, передав `input` в stdin; паникует при неудаче.
fn run_stdin_ok(args: &[&str], input: &str) {
    let mut child = Command::new("ip")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("запустить ip с stdin");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(input.as_bytes())
        .expect("передать конфиг в stdin");
    let Output { status, stderr, .. } = child.wait_with_output().expect("вывод ip");
    assert!(
        status.success(),
        "ip {} не удалась: {}",
        args.join(" "),
        String::from_utf8_lossy(&stderr).trim()
    );
}

/// Поднимает echo-сервер на `addr:0` внутри namespace `ns`.
///
/// Тестовый процесс живёт в default namespace, поэтому отдельный поток
/// входит в `ns` через `setns(2)` (network namespace — атрибут потока)
/// и биндит std-слушатель уже там; затем слушатель передаётся в tokio
/// (epoll работает с сокетами любого namespace, а `accept` наследует
/// namespace слушателя). Возвращает адрес слушателя.
fn spawn_echo_in_ns(ns: &str, addr: Ipv4Addr) -> io::Result<SocketAddr> {
    let handle = tokio::runtime::Handle::current();
    let (tx, rx) = std::sync::mpsc::channel();
    let ns_file_path = format!("/var/run/netns/{ns}");

    std::thread::Builder::new()
        .name("t-be2e-echo".into())
        .spawn(move || {
            let outcome = (|| -> io::Result<std::net::TcpListener> {
                let ns_file = std::fs::File::open(ns_file_path)?;
                // SAFETY: setns(2) с CLONE_NEWNET меняет network namespace
                // только этого потока; небезопасных инвариантов нет.
                if unsafe { libc::setns(ns_file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                let listener = std::net::TcpListener::bind(SocketAddrV4::new(addr, 0))?;
                listener.set_nonblocking(true)?;
                Ok(listener)
            })();
            let _ = tx.send(outcome);
        })
        .expect("запустить поток echo");

    let std_listener = rx
        .recv()
        .expect("поток echo завершился до бинда")
        .expect("бинд echo внутри namespace");
    let local = std_listener.local_addr()?;
    // from_std требует контекста рантайма с io-драйвером — входим в него.
    let listener = {
        let _guard = handle.enter();
        TcpListener::from_std(std_listener).expect("перенести echo-слушатель в tokio")
    };
    handle.spawn(echo_connections(listener));
    Ok(local)
}

/// Акцепт-цикл echo-сервера; каждое соединение — в своей задаче.
async fn echo_connections(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

/// Пытается пройти SOCKS5 CONNECT до echo через мост до успеха или
/// дедлайна (relay внутри бутылки стартует асинхронно).
async fn socks5_connect_echo_with_retry(socks5_port: u16, target: SocketAddr) {
    let deadline = Instant::now() + WAIT;
    loop {
        match socks5_connect_echo(socks5_port, target).await {
            Ok(()) => return,
            Err(error) => {
                if Instant::now() >= deadline {
                    panic!(
                        "SOCKS5 CONNECT через мост 127.0.0.1:{socks5_port} к {target} \
                         не прошёл за {WAIT:?}: {error:?}"
                    );
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Один цикл: согласование метода → CONNECT на echo → обмен байтами
/// в обе стороны. Любое отклонение — ошибка (для retry-обёртки).
async fn socks5_connect_echo(socks5_port: u16, target: SocketAddr) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", socks5_port)).await?;

    // Согласование метода: \x05\x01\x00 → \x05\x00.
    io_step(stream.write_all(&[0x05, 0x01, 0x00])).await?;
    let mut method = [0u8; 2];
    io_step(stream.read_exact(&mut method)).await?;
    if method != [0x05, 0x00] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("некорректный ответ согласования: {method:02x?}"),
        ));
    }

    // CONNECT на IPv4-адрес echo.
    let SocketAddr::V4(target) = target else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "echo-адрес должен быть IPv4",
        ));
    };
    let [a, b, c, d] = target.ip().octets();
    let port = target.port();
    io_step(stream.write_all(&[
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
    ]))
    .await?;
    let mut reply = [0u8; 10];
    io_step(stream.read_exact(&mut reply)).await?;
    if reply[1] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("CONNECT отклонён, REP={:#04x}", reply[1]),
        ));
    }

    // Данные доходят до echo и возвращаются (обе стороны канала).
    const PAYLOAD: &[u8] = b"bottles e2e echo";
    io_step(stream.write_all(PAYLOAD)).await?;
    let mut echoed = [0u8; PAYLOAD.len()];
    io_step(stream.read_exact(&mut echoed)).await?;
    if echoed != *PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("echo вернул {echoed:02x?} вместо {PAYLOAD:02x?}"),
        ));
    }
    Ok(())
}

/// Пытается пройти через TCP-проброс бутылки (мост → relay → туннель →
/// echo) до успеха или дедлайна: relay внутри ns стартует асинхронно,
/// соединение до его готовности отвергается.
async fn forward_connect_echo_with_retry(port: u16) {
    let deadline = Instant::now() + WAIT;
    loop {
        match forward_connect_echo(port).await {
            Ok(()) => return,
            Err(error) => {
                if Instant::now() >= deadline {
                    panic!("TCP-проброс 127.0.0.1:{port} не прошёл за {WAIT:?}: {error:?}");
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Один цикл TCP-проброса: connect → echo-обмен байтами.
async fn forward_connect_echo(port: u16) -> io::Result<()> {
    let mut stream = io_step(TcpStream::connect(("127.0.0.1", port))).await?;
    const PAYLOAD: &[u8] = b"bottles fwd echo";
    io_step(stream.write_all(PAYLOAD)).await?;
    let mut echoed = [0u8; PAYLOAD.len()];
    io_step(stream.read_exact(&mut echoed)).await?;
    if echoed != *PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("echo вернул {echoed:02x?} вместо {PAYLOAD:02x?}"),
        ));
    }
    Ok(())
}

/// Ограничивает сетевой шаг клиента таймаутом; Elapsed разворачивается
/// в io::Error (TimedOut) — чтобы retry-обёртка видела единый тип.
async fn io_step<T>(step: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    match tokio::time::timeout(Duration::from_secs(5), step).await {
        Ok(outcome) => outcome,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "таймаут сетевого шага клиента",
        )),
    }
}

/// Завершает дочерний процесс: kill + wait (SIGKILL, без сирот).
fn kill_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
