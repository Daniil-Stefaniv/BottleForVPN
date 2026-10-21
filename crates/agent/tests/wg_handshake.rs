//! Интеграционный тест WireGuard-туннеля (задача 10).
//!
//! Работает только на Linux под root (CI-джоба `agent-integration`;
//! `wireguard-tools` + `wireguard-go`). Стенд:
//!
//! ```text
//! [default ns: хост]            [ns t-wg-srv]              [ns bottle-t-wg]
//!  p<idx> 10.77.idx.1 ←veth→    t-wg-s 10.77.253.1/30  ←veth→ b<idx> 10.77.idx.2
//!  t-wg-h 10.77.253.2/30 ←──────┘      wghs 10.99.0.1        wg<idx> 10.99.0.2
//!  SOCKS5-мост 0.0.0.0:49201             echo на 10.99.0.1      relay + туннель
//! ```
//!
//! Хост маршрутизирует бутылку ↔ srv (ip_forward); бутылка поднимает
//! туннель `tunnel_up_wg` до endpoint `10.77.253.1:51820` (IP — резолв
//! замыкается на dotted-IPv4), эхо-проверка идёт SOCKS5-CONNECT'ом
//! через мост → relay → wg<idx> → wghs → echo. Запуск:
//!
//! ```sh
//! sudo -E cargo test -p agent --test wg_handshake -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::future::Future;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd as _;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agent::netns::{netns_down, netns_up, SlotTable};
use agent::wg::{parse_wg_config, tunnel_down, tunnel_up_wg};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Идентификатор тестовой бутылки (namespace `bottle-t-wg`).
const ID: &str = "t-wg";
/// Namespace «VPN-сервера». Имя без префикса `bottle-`, чтобы остатки
/// аварийного прогона не ломали ассерты теста netns_lifecycle.
const NS_SRV: &str = "t-wg-srv";
/// Хостовой конец veth до srv; второй конец живёт в srv.
const VETH_HOST: &str = "t-wg-h";
/// Конец veth внутри srv.
const VETH_SRV: &str = "t-wg-s";
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
/// Порт SOCKS5-моста бутылки в default namespace (≠ 49200 из netns_lifecycle).
const SOCKS5_PORT: u16 = 49201;
/// Общий предел ожидания асинхронных событий (relay, echo).
const WAIT: Duration = Duration::from_secs(15);
/// Пауза между попытками.
const RETRY: Duration = Duration::from_millis(100);
/// Предел ожидания готовности wireguard-go в srv.
const WG_READY_WAIT: Duration = Duration::from_secs(5);
/// Пауза между опросами готовности.
const WG_READY_POLL: Duration = Duration::from_millis(50);

/// Полный сценарий: подъём туннеля с handshake и сквозной обмен данными
/// SOCKS5 → relay → wg<idx> → wghs → echo внутри srv-namespace.
#[tokio::test]
#[ignore = "нужны root, iproute2 и wireguard (CI-джоба agent-integration)"]
async fn wg_handshake_and_socks5_over_tunnel() {
    assert_root();
    // Relay внутри бутылки — собранный bottle-agent (шов задачи 9).
    std::env::set_var("BOTTLE_AGENT_RELAY_EXE", env!("CARGO_BIN_EXE_bottle-agent"));
    cleanup_leftovers();

    // Хост маршрутизирует бутылку ↔ srv: оба veth кончаются в default ns.
    sysctl("net.ipv4.ip_forward", "1");
    // Docker на GitHub-раннерах держит политику FORWARD=DROP.
    allow_test_forward();

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
    relax_rp_filter(VETH_HOST);
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

    // (3) Сервер WireGuard в srv: wireguard-go wghs + setconf через stdin.
    // Имя wghs не пересекается с бутылочными wg<idx> (слот 1 → wg1):
    // UAPI-сокеты живут в общей файловой системе /var/run/wireguard.
    // WG_PROCESS_FOREGROUND=1: без неё wireguard-go демонизируется, и
    // wait_wg_ready видит «смерть» родителя («умер при старте»).
    let mut srv_wg = Command::new("ip")
        .args(["netns", "exec", NS_SRV, "wireguard-go", "wghs"])
        .env("WG_PROCESS_FOREGROUND", "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("запустить wireguard-go в srv");
    wait_wg_ready(NS_SRV, "wghs", &mut srv_wg);
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
            "wghs",
            "/dev/stdin",
        ],
        &srv_conf,
    );
    run_ok(
        "ip",
        &["netns", "exec", NS_SRV, "ip", "link", "set", "wghs", "up"],
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
            "wghs",
        ],
    );
    // Ответы echo клиенту уходят через wghs (AllowedIPs не создают маршруты).
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
            "wghs",
        ],
    );

    // (4) Echo на 10.99.0.1 внутри srv: поток входит в namespace (setns)
    // и биндит сокет; акцепт-цикл живёт в tokio-рантайме теста.
    let echo_addr = spawn_echo_in_ns(NS_SRV, WG_SRV_ADDR).expect("поднять echo в srv");

    // (5) Бутылка: netns_up (мост SOCKS5) + туннель.
    let mut slots = SlotTable::default();
    let slot = slots.alloc().expect("свободный слот");
    let mut handle = netns_up(&slot, ID, &[], SOCKS5_PORT)
        .await
        .expect("netns_up");
    relax_rp_filter(&format!("p{}", slot.idx));

    let cli_conf = format!(
        "[Interface]\n\
         Address = {WG_CLI_ADDR}/32\n\
         PrivateKey = {cli_priv}\n\
         \n\
         [Peer]\n\
         PublicKey = {srv_pub}\n\
         Endpoint = {SRV_VETH_ADDR}:{WG_PORT}\n\
         AllowedIPs = {WG_SRV_ADDR}/32\n\
         PersistentKeepalive = 5\n"
    );
    let cfg = parse_wg_config(&cli_conf).expect("разобрать клиентский конфиг");

    // (6) Туннель: handshake-wait ≤30 с внутри tunnel_up_wg.
    tunnel_up_wg(&mut handle, cfg)
        .await
        .expect("tunnel_up_wg: handshake не состоялся за 30 с");

    // (7) Сквозной путь: SOCKS5 CONNECT через мост → relay → wg<idx> → wghs → echo.
    socks5_connect_echo_with_retry(SOCKS5_PORT, echo_addr).await;

    // (8) Разбор: туннель, бутылка, srv-стенд.
    tunnel_down(&mut handle);
    assert!(
        handle.tunnel_child.is_none(),
        "tunnel_down забирает tunnel_child"
    );
    netns_down(handle).expect("netns_down");
    kill_wait(&mut srv_wg);
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
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
        "тест требует root: sudo -E cargo test -p agent --test wg_handshake -- --ignored"
    );
}

/// Убирает остатки прошлых прогонов: namespaces, veth, каталог DNS.
fn cleanup_leftovers() {
    let _ = Command::new("ip")
        .args(["netns", "del", &format!("bottle-{ID}")])
        .status();
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
    let _ = std::fs::remove_dir_all(format!("/etc/netns/bottle-{ID}"));
    // SIGKILL'нутый wireguard-go оставляет UAPI-сокет за собой.
    let _ = std::fs::remove_file("/var/run/wireguard/wghs.sock");
    remove_stale_veths();
    cleanup_foreign_stands();
}

/// Сносит хостовые veth первых слотов, оставшиеся от аварийно
/// завершённых соседних стендов (паника не доходит до разбора):
/// иначе чужой p<idx> ломает `ip link add` этого теста.
fn remove_stale_veths() {
    for idx in 1..=4u8 {
        let _ = Command::new("ip")
            .args(["link", "del", &format!("p{idx}")])
            .status();
    }
}

/// Сносит остатки СОСЕДНИХ srv-стендов этого CI-прогона: паника не
/// доходит до разборного кода, и выживший чужой namespace с сервером на
/// той же подсети 10.77.253.0/30 (duplicate route) молча съедает наши
/// handshake'и. Тесты бегут последовательно — чужих живых стендов нет.
fn cleanup_foreign_stands() {
    for ns in ["t-be2e-srv", "t-wg-srv", "t-ovpn-srv"] {
        let _ = Command::new("ip").args(["netns", "del", ns]).status();
    }
    for veth in ["t-be2e-h", "t-wg-h", "t-ovpn-h"] {
        let _ = Command::new("ip").args(["link", "del", veth]).status();
    }
    for sock in [
        "/var/run/wireguard/wgsrv.sock",
        "/var/run/wireguard/wghs.sock",
    ] {
        let _ = std::fs::remove_file(sock);
    }
    for pattern in [
        "wireguard-go wgsrv",
        "wireguard-go wghs",
        "openvpn --dev tun-srv",
    ] {
        let _ = Command::new("pkill").args(["-f", pattern]).status();
    }
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

/// Ослабляет rp_filter на интерфейсе хоста: путь бутылка→srv→echo
/// асимметричен с точки зрения интерфейма, strict rp_filter роняет его.
fn relax_rp_filter(iface: &str) {
    // Ошибки не критичны (значение по умолчанию на CI — loose/0).
    let _ = Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.all.rp_filter=0"])
        .status();
    let _ = Command::new("sysctl")
        .args(["-w", &format!("net.ipv4.conf.{iface}.rp_filter=0")])
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
        .name("t-wg-echo".into())
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
///
/// Клиент обязан быть async: мост и echo — задачи того же рантайма,
/// блокирующий std-сокет заморозил бы их в current-thread рантайме.
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
    let payload = b"bottle wg echo";
    io_step(stream.write_all(payload)).await?;
    let mut echoed = [0u8; 14];
    io_step(stream.read_exact(&mut echoed)).await?;
    if &echoed != payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("echo вернул {echoed:02x?} вместо {payload:02x?}"),
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
            "таймаут сетевого шага SOCKS5-клиента",
        )),
    }
}

/// Завершает дочерний процесс: kill + wait (SIGKILL, без сирот).
fn kill_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
