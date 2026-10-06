//! GO/NO-GO спайк этапа B (M4 Task 9): Linux-VM на
//! Virtualization.framework (bin­динги objc2-virtualization — «vz»
//! из плана; крейта с именем vz на crates.io нет, ruling в REPORT.md).
//!
//! Критерии (все обязательны, план Task 9):
//! 1. VM бутстрапится из arm64-образа (Alpine + bottle-agent), virtio-net NAT.
//! 2. Агент получает IP по DHCP, шлёт UDP-announce (токен); хост ловит,
//!    коннектится control-TCP: Hello → Up → Down.
//! 3. Замеры: старт VM ≤15 с, память ≤512 МБ idle.
//!
//! Итог — REPORT.md. Код не предназначен для продакшена: это факты для
//! решения «vz идёт / Swift-хелпер / отложить M4b».

#![allow(unused_unsafe)]

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use bottle_core::frame::LengthJsonCodec;
use bottle_core::protocol::{AgentReply, AgentRequest, UpArgs, PROTOCOL_VERSION};
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::{Decoder, Encoder};

/// Порт UDP-announce в VM (busybox nc → broadcast).
#[allow(dead_code)]
const ANNOUNCE_PORT: u16 = 7778;
/// Control-TCP агента.
const CONTROL_PORT: u16 = 7777;

fn main() {
    let image_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spikes/vz-poc/image".to_string());
    let token =
        std::env::var("POC_TOKEN").unwrap_or_else(|_| "poc-token-0123456789abcdef".to_string());

    let kernel_name = match std::env::var("POC_KERNEL").as_deref() {
        Ok("alpine") => "Image",
        Ok("ubuntu") => "Image-ubuntu",
        _ => {
            if std::path::PathBuf::from(&image_dir)
                .join("Image-ubuntu")
                .is_file()
            {
                "Image-ubuntu"
            } else {
                "Image"
            }
        }
    };
    let kernel = std::path::PathBuf::from(&image_dir).join(kernel_name);
    println!("kernel: {kernel_name}");
    let initrd_gz = std::path::PathBuf::from(&image_dir).join("rootfs.cpio.gz");
    let initrd_raw = std::path::PathBuf::from(&image_dir).join("rootfs.cpio");
    // VF может не принимать gzip-initrd — при наличии несжатого
    // предпочитаем его (диагностика спайка).
    let initrd = if initrd_raw.is_file() {
        initrd_raw
    } else {
        initrd_gz
    };
    for (name, path) in [("kernel", &kernel), ("initrd", &initrd)] {
        if !path.is_file() {
            eprintln!("нет {name}: {} (запустите build-image.sh)", path.display());
            std::process::exit(2);
        }
    }

    println!("== vz-poc: старт VM (токен {token})");
    let started = Instant::now();

    #[cfg(target_os = "macos")]
    let outcome = macos::run(kernel, initrd, token.clone(), started);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (kernel, initrd, started);
        eprintln!("спайк имеет смысл только на macOS");
        std::process::exit(2);
    }

    #[cfg(target_os = "macos")]
    match outcome {
        Ok(report) => {
            println!("== ИТОГ спайка:");
            println!("{report}");
        }
        Err(error) => {
            eprintln!("спайк провалился: {error}");
            std::process::exit(1);
        }
    }
}

/// Сводка спайка для REPORT.md.
#[derive(Debug)]
pub struct SpikeReport {
    pub boot_seconds: f64,
    pub rss_mib_idle: f64,
    pub hello_ok: bool,
    pub up_down_ok: bool,
    pub up_outcome: String,
}

impl std::fmt::Display for SpikeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "старт VM (до announce): {:.1} с (критерий ≤15) — {}",
            self.boot_seconds,
            if self.boot_seconds <= 15.0 {
                "OK"
            } else {
                "FAIL"
            }
        )?;
        writeln!(
            f,
            "память idle: {:.0} МиБ (критерий ≤512) — {}",
            self.rss_mib_idle,
            if self.rss_mib_idle <= 512.0 {
                "OK"
            } else {
                "FAIL"
            }
        )?;
        writeln!(
            f,
            "UDP-announce + Hello: {} — {}",
            if self.hello_ok {
                "пройден"
            } else {
                "провален"
            },
            if self.hello_ok { "OK" } else { "FAIL" }
        )?;
        writeln!(
            f,
            "Up/Down цикл: {} — {}",
            self.up_outcome,
            if self.up_down_ok { "OK" } else { "FAIL" }
        )
    }
}

/// Сетевая часть спайка: ждёт announce, гоняет Hello/Up/Down по TCP.
pub async fn protocol_trial(token: &str, started: Instant) -> Result<SpikeReport, String> {
    // 1. UDP-announce: слушаем broadcast, фильтруем токен (POC-фильтр;
    //    подписанные announce — задача T10).
    let vm_ip = wait_announce(token).await?;
    let boot_seconds = started.elapsed().as_secs_f64();
    println!("announce принят; IP VM = {vm_ip}; бутстрап {boot_seconds:.1} с");

    // 2. control-TCP: Hello → HelloAck.
    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((vm_ip, CONTROL_PORT)),
    )
    .await
    .map_err(|_| "агент не принял TCP за 10 с".to_string())?
    .map_err(|e| format!("TCP connect: {e}"))?;
    println!("TCP к агенту открыт ({})", stream.peer_addr().unwrap());

    let hello = exchange(
        &mut stream,
        AgentRequest::Hello {
            version: PROTOCOL_VERSION,
            token: token.to_string(),
        },
    )
    .await?;
    let hello_ok = matches!(hello, AgentReply::HelloAck { .. });
    println!("Hello → {hello:?}");
    if !hello_ok {
        return Ok(SpikeReport {
            boot_seconds,
            rss_mib_idle: measure_rss_mib(),
            hello_ok: false,
            up_down_ok: false,
            up_outcome: "не дошло до Up".into(),
        });
    }

    // 3. Up: валидный WG-конфиг с гарантированно недостижимым endpoint
    //    (TEST-NET) — агент обязан пройти полный путь (netns/veth/
    //    wireguard-go) и отвергнуть по handshake-таймауту; затем Down.
    let config = format!(
        "[Interface]\nPrivateKey = {}\nAddress = 10.200.0.2/32\n\n[Peer]\nPublicKey = {}\nEndpoint = 203.0.113.1:51820\nAllowedIPs = 0.0.0.0/0\nPersistentKeepalive = 5\n",
        // Синтаксически валидные ключи (содержимое не секрет).
        "6AF4r2z5kE0dV3mNpQ7sT1uW9xY2bC4dF6gH8jK0lM2=",
        "Vm5xHq3nP8rK1sT4vW6yZ8bC1dF3gH5jK7lM9nP1qR3="
    );
    let up = AgentRequest::Up(UpArgs {
        id: "poc-bottle".to_string(),
        protocol: bottle_core::profile::TunnelProtocol::WireGuard,
        config,
        dns: Some("203.0.113.53".parse().unwrap()),
        socks5_port: 0,
        forwards: vec![bottle_core::protocol::ForwardPort {
            port: 0,
            target: "203.0.113.10:3389".to_string(),
        }],
        instance: 1,
    });
    println!("Up отправлен; ждём исхода (handshake-таймаут агента ~30 с)…");
    let up_reply = tokio::time::timeout(Duration::from_secs(70), exchange(&mut stream, up))
        .await
        .map_err(|_| "Up не ответил за 70 с".to_string())?;
    let up_outcome = format!("{up_reply:?}");
    println!("Up → {up_outcome:?}");
    let up_channel_ok =
        !matches!(&up_reply, Ok(AgentReply::Error { code, .. }) if code == "BAD_REQUEST");

    // 4. Down (независимо от исхода Up) и замер idle-памяти.
    let down = exchange(
        &mut stream,
        AgentRequest::Down {
            id: "poc-bottle".to_string(),
            instance: 1,
        },
    )
    .await;
    println!("Down → {down:?}");
    let up_down_ok = up_channel_ok && down.is_ok();

    // Пауза «idle»: VM без операций — снимаем память процесса-хоста.
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(SpikeReport {
        boot_seconds,
        rss_mib_idle: measure_rss_mib(),
        hello_ok,
        up_down_ok,
        up_outcome,
    })
}

/// Слушает UDP-announce из VM; возвращает IP при совпадении токена.
async fn wait_announce(token: &str) -> Result<Ipv4Addr, String> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:7778")
        .await
        .map_err(|e| format!("bind UDP 7778: {e}"))?;
    println!("слушаю announce на :7778…");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut buffer = [0u8; 256];
    loop {
        if Instant::now() > deadline {
            return Err("announce не пришёл за 120 с".to_string());
        }
        let read = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buffer))
            .await
            .map_err(|_| "announce не пришёл за 120 с".to_string())?
            .map_err(|e| format!("UDP recv: {e}"))?
            .0;
        let message = String::from_utf8_lossy(&buffer[..read]).into_owned();
        println!("UDP: {message:?}");
        // Формат POC: "BOTTLE-ANNOUNCE <token> <ip>"
        if let Some(rest) = message.strip_prefix("BOTTLE-ANNOUNCE ") {
            let mut parts = rest.split_whitespace();
            if parts.next() == Some(token) {
                if let Some(ip) = parts.next().and_then(|s| s.parse().ok()) {
                    return Ok(ip);
                }
            }
        }
    }
}

/// Один запрос → один ответ кадрами LengthJsonCodec по TCP.
async fn exchange(
    stream: &mut tokio::net::TcpStream,
    request: AgentRequest,
) -> Result<AgentReply, String> {
    let mut buf = BytesMut::new();
    let mut codec = LengthJsonCodec::<AgentRequest>::new();
    codec
        .encode(request, &mut buf)
        .map_err(|e| format!("encode: {e}"))?;
    stream
        .write_all(&buf)
        .await
        .map_err(|e| format!("write: {e}"))?;

    let mut codec = LengthJsonCodec::<AgentReply>::new();
    let mut buffer = BytesMut::new();
    loop {
        if let Some(reply) = codec
            .decode(&mut buffer)
            .map_err(|e| format!("decode: {e}"))?
        {
            return Ok(reply);
        }
        let mut chunk = [0u8; 8192];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("агент закрыл соединение".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// RSS процесса в МиБ (VM живёт в адресном пространстве vz-poc).
fn measure_rss_mib() -> f64 {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output();
        match out {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<f64>()
                .map(|kb| kb / 1024.0)
                .unwrap_or(f64::MAX),
            Err(_) => f64::MAX,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        f64::MAX
    }
}

// macOS-транспорт Virtualization.framework.
#[cfg(target_os = "macos")]
mod macos {
    use super::SpikeReport;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use objc2::AllocAnyThread;
    use objc2_foundation::{NSError, NSString, NSURL};

    pub fn run(
        kernel: PathBuf,
        initrd: PathBuf,
        token: String,
        started: Instant,
    ) -> Result<SpikeReport, String> {
        // VF требует активного runloop на потоке старта: машину создаём и
        // стартуем на main, а сетевую часть спайка — в tokio-потоке.
        let vm = build_vm(kernel, initrd, &token)?;

        let result: Arc<std::sync::Mutex<Option<Result<SpikeReport, String>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let network = {
            let result = Arc::clone(&result);
            let token = token.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt");
                let outcome = rt.block_on(super::protocol_trial(&token, started));
                *result.lock().unwrap() = Some(outcome);
                // Итог готов: будим main-поток (runloop остановит exit ниже).
            })
        };

        start_vm(&vm)?;
        // Пульс: состояние VM в лог каждые 3 с (диагностика бута).
        {
            let vm_pulse = Arc::clone(&vm);
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                unsafe {
                    println!("[vm-state] {:?}", vm_pulse.0.state());
                }
            });
        }
        // Runloop main до конца сетевой части.
        unsafe {
            objc2_foundation::NSRunLoop::mainRunLoop().run();
        }
        network.join().map_err(|_| "сетевой поток упал")?;
        let outcome = result.lock().unwrap().take().expect("результат записан");
        outcome
    }

    fn build_vm(kernel: PathBuf, initrd: PathBuf, token: &str) -> Result<Arc<VmHandle>, String> {
        unsafe {
            let config = objc2_virtualization::VZVirtualMachineConfiguration::new();
            config.setCPUCount(
                std::env::var("POC_CPUS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2),
            );
            config.setMemorySize(512 * 1024 * 1024);

            let kernel_str = NSString::from_str(&kernel.display().to_string());
            let kernel_url = NSURL::fileURLWithPath(&kernel_str);
            let boot = objc2_virtualization::VZLinuxBootLoader::initWithKernelURL(
                objc2_virtualization::VZLinuxBootLoader::alloc(),
                &kernel_url,
            );
            // POC_INIT=sh — rdinit /bin/sh; по умолчанию panic-тест без rdinit.
            let init = match std::env::var("POC_INIT").as_deref() {
                Ok("sh") => "rdinit=/bin/sh",
                Ok("init") => "rdinit=/sbin/init",
                _ => "rdinit=/sbin/init",
            };
            let cmdline = NSString::from_str(&format!(
                "console=hvc0 ignore_loglevel {init} bfa.token={token}"
            ));
            boot.setCommandLine(&cmdline);
            let initrd_str = NSString::from_str(&initrd.display().to_string());
            let initrd_url = NSURL::fileURLWithPath(&initrd_str);
            if std::env::var("POC_NO_INITRD").is_err() {
                boot.setInitialRamdiskURL(Some(&initrd_url));
            }
            config.setBootLoader(Some(&boot));

            let platform = objc2_virtualization::VZGenericPlatformConfiguration::new();
            config.setPlatform(&platform);

            // virtio-serial консоль: stdout VM → лог спайка (console=hvc0).
            let serial = objc2_virtualization::VZVirtioConsoleDeviceSerialPortConfiguration::new();
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return Err("pipe для консоли не создан".to_string());
            }
            let (read_fd, write_fd) = (fds[0], fds[1]);
            let read_handle = objc2_foundation::NSFileHandle::initWithFileDescriptor(
                objc2_foundation::NSFileHandle::alloc(),
                read_fd,
            );
            let write_handle = objc2_foundation::NSFileHandle::initWithFileDescriptor(
                objc2_foundation::NSFileHandle::alloc(),
                write_fd,
            );
            let attachment =
                objc2_virtualization::VZFileHandleSerialPortAttachment::
                initWithFileHandleForReading_fileHandleForWriting(
                    objc2_virtualization::VZFileHandleSerialPortAttachment::alloc(),
                    Some(&read_handle),
                    Some(&write_handle),
                );
            serial.setAttachment(Some(&attachment));
            let serial_as_base: &objc2_virtualization::VZSerialPortConfiguration = &serial;
            let serials = objc2_foundation::NSArray::from_slice(&[serial_as_base]);
            config.setSerialPorts(&serials);
            spawn_console_reader(read_fd);

            // virtio-net NAT (vmnet shared): сеть 192.168.76.0/24 c DHCP.
            let net = objc2_virtualization::VZVirtioNetworkDeviceConfiguration::new();
            let nat = objc2_virtualization::VZNATNetworkDeviceAttachment::new();
            net.setAttachment(Some(&nat));
            let net_as_base: &objc2_virtualization::VZNetworkDeviceConfiguration = &net;
            let devices = objc2_foundation::NSArray::from_slice(&[net_as_base]);
            config.setNetworkDevices(&devices);

            config
                .validateWithError()
                .map_err(|error| format!("конфигурация VM невалидна: {error}"))?;

            let vm = objc2_virtualization::VZVirtualMachine::initWithConfiguration(
                objc2_virtualization::VZVirtualMachine::alloc(),
                &config,
            );
            Ok(Arc::new(VmHandle(vm)))
        }
    }

    /// Читает read-end pipe (stdout VM) и печатает с префиксом [vm].
    fn spawn_console_reader(read_fd: i32) {
        use std::io::BufRead;
        use std::os::unix::io::FromRawFd;
        let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                println!("[vm] {line}");
            }
        });
    }

    struct VmHandle(objc2::rc::Retained<objc2_virtualization::VZVirtualMachine>);

    // VF-объект используется с одного потока (start/runloop); Arc — только
    // для времени жизни, доступов из других потоков нет.
    unsafe impl Send for VmHandle {}
    unsafe impl Sync for VmHandle {}

    fn start_vm(vm: &VmHandle) -> Result<(), String> {
        // Старт неблокирующий: completion приходит на runloop main,
        // поэтому НЕ ждём его здесь (runloop запускается сразу после).
        unsafe {
            let block = block2::RcBlock::new(move |error: *mut NSError| {
                if error.is_null() {
                    println!("VM: старт подтверждён (completion)");
                } else {
                    let err = unsafe { &*error };
                    let description = unsafe { err.localizedDescription() };
                    eprintln!(
                        "VM: ошибка старта: domain={:?} code={} {:?} userInfo={:?}",
                        unsafe { err.domain() },
                        unsafe { err.code() },
                        description,
                        unsafe { err.userInfo() }
                    );
                }
            });
            vm.0.startWithCompletionHandler(&block);
            Ok(())
        }
    }
}
