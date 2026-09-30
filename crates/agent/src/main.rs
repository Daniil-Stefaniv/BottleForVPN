//! `bottle-agent` — root-исполнитель внутри WSL-аплаенса.
//!
//! Подкоманда `serve` поднимает control-сервер (TCP `0.0.0.0:7777`),
//! через который демон управляет бутылками. Подкоманда `relay` запускает
//! SOCKS5- и forward-серверы внутри network namespace бутылки — её вызывает
//! сам агент при `netns_up` (через `ip netns exec`). Флаг `--boot` появится
//! в задаче 11.

use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use agent::control::{detect_vm_ip, serve, AgentState, ControlConfig};
use agent::relay_run::{parse_forward_spec, run_relay};
use agent::token::load_or_create_token;
use bottle_core::protocol::ForwardPort;

/// Агент BottleForApps внутри WSL-апплайенса.
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
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { vm_ip } => run_serve(vm_ip).await,
        Command::Relay {
            socks5_port,
            forward,
        } => run_relay(socks5_port, forward).await,
    };
    if let Err(error) = result {
        eprintln!("bottle-agent: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Поднимает control-сервер: токен, IP аплаенса, цикл соединений.
async fn run_serve(vm_ip_override: Option<Ipv4Addr>) -> std::io::Result<()> {
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

    let state = Arc::new(AgentState::new(token, vm_ip));
    let shutdown = tokio_util::sync::CancellationToken::new();
    serve(state, shutdown).await
}
