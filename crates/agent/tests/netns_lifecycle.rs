//! Интеграционный тест жизненного цикла network namespace (задача 9).
//!
//! Работает только на Linux под root: создаёт настоящий namespace,
//! veth-пару и запускает relay — собранный `bottle-agent` (подменяется
//! через `BOTTLE_AGENT_RELAY_EXE`, потому что `current_exe()` здесь —
//! тестовый бинарник). Запуск:
//!
//! ```sh
//! sudo -E cargo test -p agent --test netns_lifecycle -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use agent::netns::{netns_down, netns_up, set_dns, SlotTable};

/// Идентификатор тестовой бутылки.
const ID: &str = "t-a";
/// Порт SOCKS5-моста в default namespace.
const SOCKS5_PORT: u16 = 49200;
/// Имя namespace, ожидаемое в выводе `ip netns list`.
const NS_NAME: &str = "bottle-t-a";
/// Общий предел ожидания асинхронных событий ядра (запуск relay,
/// разрушение namespace после `ip netns del`).
const WAIT: Duration = Duration::from_secs(15);
/// Пауза между попытками.
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Полный цикл: подъём namespace с relay внутри, SOCKS5 через мост,
/// разбор, повторный подъём на том же слоте.
///
/// multi_thread обязателен: SOCKS5-проверка ниже — блокирующий std-сокет,
/// а мост — задача того же рантайма; в current-thread рантайме
/// блокирующий клиент замораживает планировщик и ответ моста не может
/// быть прочитан (проверено минимальным воспроизведением).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "нужны root и iproute2 (запускается CI-джобой agent-integration)"]
async fn netns_up_down_lifecycle() {
    assert_root();
    // Relay внутри ns — собранный bottle-agent, а не тестовый бинарник.
    std::env::set_var("BOTTLE_AGENT_RELAY_EXE", env!("CARGO_BIN_EXE_bottle-agent"));
    cleanup_leftovers();

    let mut slots = SlotTable::default();
    let slot = slots.alloc().expect("свободный слот");

    // (а) namespace существует после netns_up.
    let handle = netns_up(&slot, ID, &[], SOCKS5_PORT)
        .await
        .expect("netns_up");
    let list = ip_output(&["netns", "list"]);
    assert!(
        list.contains(NS_NAME),
        "ip netns list не содержит {NS_NAME}: {list}"
    );

    // set_dns пишет resolv.conf в /etc/netns/<ns>.
    set_dns(ID, Some(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 1)))).expect("set_dns");
    let resolv = std::fs::read_to_string(format!("/etc/netns/{NS_NAME}/resolv.conf"))
        .expect("resolv.conf создан");
    assert!(
        resolv.contains("nameserver 10.20.30.1"),
        "в resolv.conf нет сервера: {resolv}"
    );

    // (б) SOCKS5-согласование через мост: relay жив внутри namespace.
    negotiate_socks5_with_retry(SOCKS5_PORT);

    // (в) разбор: namespace и veth исчезают, каталог DNS удаляется.
    netns_down(handle).expect("netns_down");
    let list = ip_output(&["netns", "list"]);
    assert!(
        !list.contains("bottle-"),
        "ip netns list всё ещё содержит bottle-*: {list}"
    );
    assert!(
        !Path::new(&format!("/etc/netns/{NS_NAME}")).exists(),
        "каталог /etc/netns/{NS_NAME} не удалён"
    );
    wait_veth_gone(slot.idx);

    // Повторный подъём на том же слоте работает (идемпотентность адресов).
    let handle = netns_up(&slot, ID, &[], SOCKS5_PORT)
        .await
        .expect("повторный netns_up");
    negotiate_socks5_with_retry(SOCKS5_PORT);
    netns_down(handle).expect("повторный netns_down");
    wait_veth_gone(slot.idx);
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
        "тест требует root: sudo -E cargo test -p agent --test netns_lifecycle -- --ignored"
    );
}

/// Убирает остатки прошлых прогонов: имя namespace, veth, каталог DNS.
fn cleanup_leftovers() {
    let _ = Command::new("ip").args(["netns", "del", NS_NAME]).status();
    let _ = std::fs::remove_dir_all(format!("/etc/netns/{NS_NAME}"));
}

/// Возвращает stdout команды `ip`.
fn ip_output(args: &[&str]) -> String {
    let mut cmd = Command::new("ip");
    cmd.args(args);
    let output = cmd.output().expect("запустить ip");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Пытается пройти SOCKS5-согласование через мост до успеха или дедлайна.
///
/// Relay внутри namespace стартует асинхронно: первые соединения моста
/// могут попадать в ещё не слушающий порт.
fn negotiate_socks5_with_retry(port: u16) {
    let deadline = Instant::now() + WAIT;
    loop {
        match negotiate_socks5(port) {
            Ok(()) => return,
            Err(error) => {
                if Instant::now() >= deadline {
                    panic!(
                        "SOCKS5-согласование через мост 127.0.0.1:{port} \
                         не прошло за {:?}: {error:?}",
                        WAIT
                    );
                }
                thread::sleep(RETRY_DELAY);
            }
        }
    }
}

/// Одно SOCKS5-согласование: `\x05\x01\x00` → `\x05\x00`.
fn negotiate_socks5(port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&[0x05, 0x01, 0x00])?;
    let mut method = [0u8; 2];
    stream.read_exact(&mut method)?;
    if method != [0x05, 0x00] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("некорректный ответ согласования: {method:02x?}"),
        ));
    }
    Ok(())
}

/// Ждёт исчезновения хостового veth `p<idx>`.
///
/// Разрушение namespace после `ip netns del` происходит в ядре
/// асинхронно (RCU), поэтому отсутствие интерфейса опрашивается.
fn wait_veth_gone(idx: u8) {
    let name = format!("p{idx}");
    let deadline = Instant::now() + WAIT;
    loop {
        let Output { status, .. } = Command::new("ip")
            .args(["link", "show", &name])
            .output()
            .expect("запустить ip");
        if !status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "veth {name} не удалён за {:?} после netns_down",
            WAIT
        );
        thread::sleep(RETRY_DELAY);
    }
}
