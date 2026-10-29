//! `bottle-agent` — root-исполнитель внутри WSL-аплаенса.
//!
//! Подкоманда `serve` поднимает control-сервер (TCP `0.0.0.0:7777`),
//! через которого демон управляет бутылками. Флаг `--boot` перед службой
//! чистит осиротевшие ресурсы прошлых запусков. Подкоманда `relay`
//! запускает SOCKS5- и forward-серверы внутри network namespace бутылки —
//! её вызывает сам агент при `netns_up` (через `ip netns exec`).

use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use agent::bottles::{boot_cleanup, Bottles};
use agent::control::{detect_vm_ip, serve, AgentState, ControlConfig};
use agent::relay_run::{parse_forward_spec, run_relay};
use agent::token::load_or_create_token;
use bottle_core::protocol::ForwardPort;

/// Агент Погребка внутри WSL-апплайенса.
#[derive(Debug, Parser)]
#[command(name = "bottle-agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Запустить control-сервер на 0.0.0.0:7777.
    Serve {
        /// IP аплаенса в сети WSL NAT; по умолчанию определяется
        /// по маршруту по умолчанию. Переопределяется в тестах.
        #[arg(long, value_name = "IP")]
        vm_ip: Option<Ipv4Addr>,
        /// Перед службой разобрать осиротевшие бутылки прошлых запусков
        /// (процессы, namespaces, tmpfs-каталоги) и поставить masquerade.
        #[arg(long)]
        boot: bool,
    },
    /// Запустить relay (SOCKS5 + пробросы) внутри namespace бутылки.
    ///
    /// Вызывается самим агентом через `ip netns exec`; вручную запускать
    /// не нужно.
    Relay {
        /// Порт SOCKS5-сервера внутри namespace.
        #[arg(long, value_name = "PORT")]
        socks5_port: u16,
        /// TCP-проброс вида `ПОРТ:ЦЕЛЬ`; можно повторять.
        #[arg(long, value_name = "PORT:TARGET", value_parser = parse_forward_spec)]
        forward: Vec<ForwardPort>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { vm_ip, boot } => run_serve(vm_ip, boot).await,
        Command::Relay {
            socks5_port,
            forward,
        } => run_relay(socks5_port, forward).await,
    };
    if let Err(error) = result {
        tracing::error!(%error, "bottle-agent завершился с ошибкой");
        eprintln!("bottle-agent: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Инициализирует логирование: уровень из `RUST_LOG`, по умолчанию `info`.
///
/// Вызывается до разбора аргументов — ошибки запуска тоже логируются
/// (до сих пор warn-логи агента были невидимы без subscriber'а).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Поднимает control-сервер: токен, IP аплаенса, цикл соединений.
///
/// С `boot` сначала [`boot_cleanup`] — осиротевшие процессы и остатки
/// namespaces прошлых запусков чинятся до приёма команд демона.
async fn run_serve(vm_ip_override: Option<Ipv4Addr>, boot: bool) -> std::io::Result<()> {
    let config = ControlConfig::default();

    let vm_ip = match vm_ip_override {
        Some(ip) => ip,
        None => detect_vm_ip().map_err(|e| {
            std::io::Error::new(e.kind(), format!("не удалось определить IP аплаенса: {e}"))
        })?,
    };
    let token = load_or_create_token(&config.token_path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "не удалось загрузить токен из {}: {e}",
                config.token_path.display()
            ),
        )
    })?;

    if boot {
        tracing::info!("--boot: очистка остатков прошлых запусков");
        boot_cleanup();
    }

    let state = Arc::new(AgentState::new(token, vm_ip, Arc::new(Bottles::new())));
    let shutdown = tokio_util::sync::CancellationToken::new();
    serve(state, shutdown).await
}
