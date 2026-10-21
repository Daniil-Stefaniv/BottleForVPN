//! Интеграционный тест OpenVPN-туннеля (задача 14).
//!
//! Работает только на Linux под root (CI-джоба `agent-integration`;
//! пакет `openvpn`). Стенд по мотивам wg_handshake/bottles_e2e —
//! «VPN-сервер» в отдельном namespace, бутылка поднимается через
//! `Bottles::up` с протоколом OpenVPN:
//!
//! ```text
//! [default ns: хост]            [ns t-ovpn-srv]            [ns bottle-t-ovpn]
//!  p<idx> 10.77.idx.1 ←veth→    t-ovpn-s 10.77.253.1/30 ←veth→ b<idx> 10.77.idx.2
//!  t-ovpn-h 10.77.253.2/30 ←────┘  openvpn tun-srv           openvpn tun0
//!  мост 0.0.0.0:49220              10.99.3.1, echo           10.99.3.2
//! ```
//!
//! Статический ключ (`openvpn --genkey secret`) кладётся в общий tmpfs
//! `/tmp` — файловая система одна на все namespaces, обе стороны читают
//! один файл. Сервер в srv-netns слушает UDP 1194; клиент бутылки
//! коннектится на endpoint `10.77.253.1:1194` (srv-конец veth, хост
//! маршрутизирует). Критерий: агент вернул `Ready` (tun0 в бутылке
//! появился ≤60 с — контракт `openvpn_up`) + сквозной SOCKS5 CONNECT
//! через мост → relay → tun0 → tun-srv → echo. Запуск:
//!
//! ```sh
//! sudo -E cargo test -p agent --test openvpn_e2e -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd as _;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agent::bottles::Bottles;
use bottle_core::profile::TunnelProtocol;
use bottle_core::protocol::{AgentReply, UpArgs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Идентификатор тестовой бутылки (namespace `bottle-t-ovpn`).
const ID: &str = "t-ovpn";
/// Namespace «VPN-сервера». Имя без префикса `bottle-`, чтобы остатки
/// аварийного прогона не ломали ассерты теста netns_lifecycle.
const NS_SRV: &str = "t-ovpn-srv";
/// Хостовый конец veth до srv; второй конец живёт в srv.
const VETH_HOST: &str = "t-ovpn-h";
/// Конец veth внутри srv.
const VETH_SRV: &str = "t-ovpn-s";
/// Адрес srv-конца veth — endpoint туннеля (подсеть 253 вне слотов 1..=250).
const SRV_VETH_ADDR: Ipv4Addr = Ipv4Addr::new(10, 77, 253, 1);
/// Адрес хостового конца veth до srv.
const HOST_VETH_ADDR: Ipv4Addr = Ipv4Addr::new(10, 77, 253, 2);
/// Внутренний адрес tun-интерфейса сервера (на нём слушает echo).
const TUN_SRV_ADDR: Ipv4Addr = Ipv4Addr::new(10, 99, 3, 1);
/// Внутренний адрес tun-интерфейса клиента (бутылки).
const TUN_CLI_ADDR: Ipv4Addr = Ipv4Addr::new(10, 99, 3, 2);
/// Имя tun-интерфейса сервера (клиентский в бутылке — `tun0`, его
/// поднимает агент с `--dev tun0`).
const SRV_TUN: &str = "tun-srv";
/// Порт UDP OpenVPN-сервера в srv.
const OVPN_PORT: u16 = 1194;
/// Порт SOCKS5-моста бутылки в default namespace (продолжение нумерации:
/// 49200 netns_lifecycle, 49201 wg_handshake, 49210/49211 bottles_e2e).
const SOCKS5_PORT: u16 = 49220;
/// Статический ключ OpenVPN: общий tmpfs, файл читают обе стороны
/// в разных namespaces.
const KEY_FILE: &str = "/tmp/bottle-ovpn-e2e-static.key";
/// Instance-поколение демона в этом стенде (протокол v2).
const INSTANCE: u64 = 7;
/// Общий предел ожидания асинхронных событий (relay, echo).
const WAIT: Duration = Duration::from_secs(15);
/// Пауза между попытками.
const RETRY: Duration = Duration::from_millis(100);
/// Предел ожидания готовности openvpn-сервера (tun-srv в srv-netns).
const SRV_READY_WAIT: Duration = Duration::from_secs(15);
/// Пауза между опросами готовности сервера.
const SRV_READY_POLL: Duration = Duration::from_millis(250);

/// Полный сценарий: подъём OpenVPN-бутылки через `Bottles::up` со
/// статическим ключом и сквозной обмен SOCKS5 → relay → tun0 → tun-srv →
/// echo внутри srv-namespace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "нужны root, iproute2 и openvpn (CI-джоба agent-integration)"]
async fn openvpn_static_key_tunnel_and_socks5_over_it() {
    assert_root();
    // Relay внутри бутылки — собранный bottle-agent (шов задачи 9).
    std::env::set_var("BOTTLE_AGENT_RELAY_EXE", env!("CARGO_BIN_EXE_bottle-agent"));
    cleanup_leftovers();

    // Хост маршрутизирует бутылку ↔ srv: оба veth кончаются в default ns.
    sysctl("net.ipv4.ip_forward", "1");
    // Docker на GitHub-раннерах держит политику FORWARD=DROP.
    allow_test_forward();
    // rp_filter ослабляем до создания интерфейсов бутылки: veth появляется
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

    // (2) Статический ключ: генерация на стенде, права 0600.
    genkey();

    // (3) OpenVPN-сервер в srv: spawn (не --daemon) + ожидание tun-srv —
    // живой процесс держит namespace, а его смерть видна ранним выходом.
    let mut srv_ovpn = Command::new("ip")
        .args([
            "netns",
            "exec",
            NS_SRV,
            "openvpn",
            "--dev",
            SRV_TUN,
            "--dev-type",
            "tun",
            "--proto",
            "udp",
            "--port",
            &OVPN_PORT.to_string(),
            "--ifconfig",
            &TUN_SRV_ADDR.to_string(),
            &TUN_CLI_ADDR.to_string(),
            "--secret",
            KEY_FILE,
            // OpenVPN 2.6 + OpenSSL 3: дефолтный BF-CBC выкинут из
            // провайдера («Cipher BF-CBC not supported»), задаём явно.
            "--cipher",
            "AES-256-CBC",
        ])
        // Лог openvpn наследует stdio теста — окно диагностики
        // «Initialization Sequence Completed».
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("запустить openvpn в srv");
    wait_tun_ready(NS_SRV, SRV_TUN, &mut srv_ovpn);

    // (4) Echo на 10.99.3.1 внутри srv: поток входит в namespace (setns)
    // и биндит сокет; акцепт-цикл живёт в tokio-рантайме теста.
    let echo_addr = spawn_echo_in_ns(NS_SRV, TUN_SRV_ADDR).expect("поднять echo в srv");

    // (5) Бутылка через публичное API: протокол OpenVpn, статический ключ
    // из общего tmpfs. `dev` в конфиге не задаётся — агент поднимает
    // tun0 сам (`--dev tun0 --dev-type tun`), конфиг задаёт только
    // endpoint/адресацию/ключ.
    let bottles = Bottles::new();
    let args = UpArgs {
        id: ID.to_string(),
        protocol: TunnelProtocol::OpenVpn,
        config: client_config(),
        dns: None,
        socks5_port: SOCKS5_PORT,
        forwards: Vec::new(),
        instance: INSTANCE,
        username: None,
        password: None,
    };
    let reply = bottles.up(args).await;
    assert_eq!(
        reply,
        AgentReply::Ready { id: ID.to_string() },
        "up → Ready"
    );

    // Ready означает tun0 в бутылке (агент ждёт его ≤60 с); сверяем
    // адресацию явно — критерий задачи.
    let tun0 = run_ok(
        "ip",
        &[
            "netns",
            "exec",
            &format!("bottle-{ID}"),
            "ip",
            "addr",
            "show",
            "tun0",
        ],
    );
    assert!(
        tun0.contains(&TUN_CLI_ADDR.to_string()),
        "на tun0 нет адреса {TUN_CLI_ADDR}: {tun0}"
    );

    // (6) Сквозной путь: SOCKS5 CONNECT через мост 49220 → relay →
    // tun0 → tun-srv → echo.
    socks5_connect_echo_with_retry(SOCKS5_PORT, echo_addr).await;

    // (7) Down своего поколения → Ok; namespace и tmpfs-каталог разобраны.
    let reply = bottles.down(ID, INSTANCE).await;
    assert_eq!(reply, AgentReply::Ok, "down → Ok");
    let list = ip_netns_list();
    assert!(
        !list.contains("bottle-"),
        "после down в ip netns list не должно быть bottle-*: {list}"
    );
    assert!(
        !std::path::Path::new("/run/bottles").join(ID).exists(),
        "tmpfs-каталог /run/bottles/{ID} не удалён после down"
    );

    // (8) Разбор стенда: сервер, namespace, veth, ключ.
    kill_wait(&mut srv_ovpn);
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
    let _ = std::fs::remove_file(KEY_FILE);
}

/// Клиентский конфиг OpenVPN: endpoint — адрес srv-конца veth
/// (dotted-IPv4 — резолв замыкается без DNS), p2p-адресация на пару
/// 10.99.3.2/10.99.3.1, статический ключ из общего tmpfs.
fn client_config() -> String {
    format!(
        "proto udp\n\
         remote {SRV_VETH_ADDR} {OVPN_PORT}\n\
         nobind\n\
         ifconfig {TUN_CLI_ADDR} {TUN_SRV_ADDR}\n\
         secret {KEY_FILE}\n\
         cipher AES-256-CBC\n"
    )
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
        "тест требует root: sudo -E cargo test -p agent --test openvpn_e2e -- --ignored"
    );
}

/// Убирает остатки прошлых прогонов: namespaces, veth, каталоги DNS и
/// tmpfs, статический ключ.
fn cleanup_leftovers() {
    let _ = Command::new("ip")
        .args(["netns", "del", &format!("bottle-{ID}")])
        .status();
    let _ = std::fs::remove_dir_all(format!("/etc/netns/bottle-{ID}"));
    let _ = std::fs::remove_dir_all(format!("/run/bottles/{ID}"));
    let _ = Command::new("ip").args(["netns", "del", NS_SRV]).status();
    let _ = Command::new("ip").args(["link", "del", VETH_HOST]).status();
    let _ = std::fs::remove_file(KEY_FILE);
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

/// Выполняет команду, паникуя с stderr при неудаче; возвращает stdout
/// (для проверок вывода — адреса интерфейсов).
fn run_ok(program: &str, args: &[&str]) -> String {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("запустить {program} {}: {error}", args.join(" ")));
    assert!(
        status.success(),
        "{program} {} не удалась: {}",
        args.join(" "),
        String::from_utf8_lossy(&stderr).trim()
    );
    String::from_utf8_lossy(&stdout).into_owned()
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

/// Генерирует статический ключ OpenVPN (`openvpn --genkey secret`) в
/// [`KEY_FILE`] с правами 0600.
fn genkey() {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new("openvpn")
        .args(["--genkey", "secret", KEY_FILE])
        .output()
        .expect("запустить openvpn --genkey");
    assert!(
        status.success(),
        "openvpn --genkey secret {KEY_FILE} не удался: {}{}",
        String::from_utf8_lossy(&stderr).trim(),
        String::from_utf8_lossy(&stdout).trim(),
    );
    set_mode_0600(std::path::Path::new(KEY_FILE));
}

/// Права 0600 (unix): ключ — секрет.
fn set_mode_0600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|error| panic!("права 0600 на {}: {error}", path.display()));
}

/// Ждёт появления tun-интерфейса в namespace: `ip link show` успешен
/// только на существующем интерфейсе. Ранний выход при смерти процесса.
fn wait_tun_ready(ns: &str, iface: &str, child: &mut Child) {
    let deadline = Instant::now() + SRV_READY_WAIT;
    loop {
        let ready = Command::new("ip")
            .args(["netns", "exec", ns, "ip", "link", "show", iface])
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
        assert!(!dead, "openvpn в {ns} умер при старте");
        assert!(
            Instant::now() < deadline,
            "интерфейс {iface} не появился за {SRV_READY_WAIT:?} в {ns}"
        );
        std::thread::sleep(SRV_READY_POLL);
    }
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
        .name("t-ovpn-echo".into())
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
/// блокирующий std-сокет заморозил бы их.
async fn socks5_connect_echo_with_retry(socks5_port: u16, target: SocketAddr) {
    let deadline = Instant::now() + WAIT;
    loop {
        match socks5_connect_echo(socks5_port, target).await {
            Ok(()) => return,
            Err(error) => {
                if Instant::now() >= deadline {
                    panic!(
                        "SOCKS5 CONNECT через мост 127.0.0.1:{socks5_port} к {target} \
                         не прошёл за {WAIT:?}: {error:?}\n{}",
                        tunnel_diag()
                    );
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Снимок диагностики туннеля при непроходе трафика: хвост лога
/// openvpn-клиента бутылки + адреса/маршруты обоих namespace.
/// Видно, завершился ли «Initialization Sequence» и куда смотрит 10.99.3.0/24.
fn tunnel_diag() -> String {
    let log = std::fs::read_to_string(format!("/run/bottles/{ID}/openvpn.log"))
        .map(|text| {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            lines[lines.len().saturating_sub(12)..].join("\n")
        })
        .unwrap_or_else(|e| format!("<лог не прочитан: {e}>"));
    let grab = |ns: &str| -> String {
        Command::new("ip")
            .args(["netns", "exec", ns, "ip", "-o", "addr"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .replace('\n', "; ")
            })
            .unwrap_or_else(|e| format!("<{e}>"))
    };
    format!(
        "лог клиента (хвост):\n{log}\nадреса bottle-{ID}: {}\nадреса {NS_SRV}: {}",
        grab(&format!("bottle-{ID}")),
        grab(NS_SRV),
    )
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
    const PAYLOAD: &[u8] = b"bottle openvpn echo";
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
            "таймаут сетевого шага SOCKS5-клиента",
        )),
    }
}

/// Завершает дочерний процесс: kill + wait (SIGKILL, без сирот).
fn kill_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
