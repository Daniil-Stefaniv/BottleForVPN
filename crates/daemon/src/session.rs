//! Сессия канала управления (0.3, спека §5): единая точка протокола
//! для named pipe (Windows) и UDS (macOS/Unix).
//!
//! Проводной формат — кадры `serde_json::Value` (LengthJsonCodec):
//! значение сначала пробуется как [`GuiRequest`] (протокол GUI),
//! затем как legacy [`CliRequest`] (служебные команды старого CLI до
//! его сокращения). Ответы и события кодируются тем же нейтральным
//! кадром.
//!
//! Auth-гейт control-соединения: до `Hello { token }` с верным токеном
//! допустимы только служебные `Doctor`/`LogsExport` (их шлёт CLI без
//! токена); остальное отклоняется `AUTH: …`. Неверный токен — ошибка
//! и закрытие соединения. `Subscribe { token }` после проверки
//! переводит соединение в event-режим: исходящий поток
//! [`bottle_core::ipc::DaemonEvent`]
//! из broadcast-канала демона (отставший подписчик пропускает события
//! с warn-логом, соединение не рвётся).

use std::sync::Arc;

use bottle_core::events::Src;
use bottle_core::frame::LengthJsonCodec;
use bottle_core::ipc::{
    CliReply, CliRequest, DaemonStats, GuiReply, GuiRequest, AUTH_ERR,
};
use bytes::BytesMut;
use tokio::io::{split, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::codec::{Encoder, FramedRead};
use tokio_util::sync::CancellationToken;

use crate::Daemon;

/// Переходный ответ, когда менеджер бутылок не подключён (тесты и
/// dev-сборки без аплаенса).
pub(crate) const MANAGER_NOT_WIRED: &str = "менеджер бутылок не подключён";

/// Обслуживает одно соединение канала управления до отключения клиента,
/// ошибки протокола или отмены `shutdown`.
pub(crate) async fn handle_connection<S>(
    stream: S,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) where
    S: AsyncRead + AsyncWrite,
{
    let (read_half, mut write_half) = split(stream);
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<serde_json::Value>::new());
    let mut authed = false;

    loop {
        let frame = tokio::select! {
            maybe = reader.next() => match maybe {
                Some(Ok(value)) => value,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "некорректный кадр, соединение закрывается");
                    break;
                }
                None => break,
            },
            () = shutdown.cancelled() => break,
        };

        // Subscribe — смена режима соединения (обрабатывается до
        // общего гейта: несёт собственный токен).
        if let Ok(GuiRequest::Subscribe { token }) =
            serde_json::from_value::<GuiRequest>(frame.clone())
        {
            if token != daemon.gui_token {
                let _ = send_value(
                    &mut write_half,
                    &GuiReply::Error {
                        message: format!("{AUTH_ERR}: неверный токен"),
                    },
                )
                .await;
                break;
            }
            if send_value(&mut write_half, &GuiReply::Ok).await.is_err() {
                break;
            }
            run_subscription(&mut reader, &mut write_half, &daemon, &shutdown).await;
            break;
        }

        let reply: serde_json::Value = if let Ok(req) = serde_json::from_value::<GuiRequest>(frame.clone()) {
            match req {
                GuiRequest::Hello { token } => {
                    if token == daemon.gui_token {
                        authed = true;
                        to_value(&GuiReply::Ok)
                    } else {
                        let _ = send_value(
                            &mut write_half,
                            &GuiReply::Error {
                                message: format!("{AUTH_ERR}: неверный токен"),
                            },
                        )
                        .await;
                        break;
                    }
                }
                // Служебные команды доступны без токена (их шлёт CLI,
                // у которого токена нет — спека §5).
                GuiRequest::Doctor | GuiRequest::LogsExport { .. } => {
                    to_value(&gui_dispatch(&daemon, Some(&shutdown), req).await)
                }
                _ if !authed => to_value(&GuiReply::Error {
                    message: format!("{AUTH_ERR}: сначала Hello с токеном"),
                }),
                _ => {
                    let reply = tokio::select! {
                        reply = gui_dispatch(&daemon, Some(&shutdown), req) => reply,
                        () = shutdown.cancelled() => break,
                    };
                    to_value(&reply)
                }
            }
        } else if let Ok(req) = serde_json::from_value::<CliRequest>(frame) {
            let reply = tokio::select! {
                reply = crate::pipe::dispatch(&daemon, Some(&shutdown), req) => reply,
                () = shutdown.cancelled() => break,
            };
            to_value(&reply)
        } else {
            tracing::warn!("кадр не распознан ни как GuiRequest, ни как CliRequest");
            break;
        };

        if send_value(&mut write_half, &reply).await.is_err() {
            break;
        }
    }
}

/// Режим подписки: исходящие кадры DaemonEvent до отключения клиента
/// или остановки демона. Входящие кадры в этом режиме — повод закрыть
/// соединение (протокол подписки односторонний).
async fn run_subscription<R, W>(
    reader: &mut FramedRead<R, LengthJsonCodec<serde_json::Value>>,
    write_half: &mut W,
    daemon: &Daemon,
    shutdown: &CancellationToken,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut rx = daemon.events.subscribe();
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) => {
                    if send_value(write_half, &event).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "подписчик отстал: события пропущены");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = reader.next() => {
                match incoming {
                    None => break, // клиент отключился
                    Some(_) => {
                        tracing::warn!("кадр от подписчика: подписка односторонняя, соединение закрывается");
                        break;
                    }
                }
            }
            () = shutdown.cancelled() => break,
        }
    }
}

/// Кодирует и отправляет кадр Value (fallible-путь `Encoder`).
async fn send_value<W, T: serde::Serialize>(write_half: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    LengthJsonCodec::<serde_json::Value>::new()
        .encode(to_value(value), &mut buf)
        .map_err(std::io::Error::other)?;
    write_half.write_all(&buf).await
}

/// Сериализация в нейтральный кадр; типы протокола сериализуются
/// всегда, поэтому expect безопасен.
fn to_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("типы протокола сериализуются")
}

/// Публикует событие реестра (спека §6) подписчикам; отсутствие
/// подписчиков — не ошибка.
/// Пишет событие реестра в событийный журнал демона (файл + push
/// подписчикам). Тонкая обёртка над [`crate::logging::emit`]: источник
/// всегда daemon.
pub(crate) fn emit(daemon: &Daemon, code: &str, ctx: serde_json::Map<String, serde_json::Value>) {
    crate::logging::emit(&daemon.events, &daemon.event_log, Src::Daemon, code, ctx);
}

/// Диспетчер протокола GUI (control-соединение после auth-гейта;
/// `Doctor`/`LogsExport` допускаются и до него).
pub(crate) async fn gui_dispatch(
    daemon: &Daemon,
    shutdown: Option<&CancellationToken>,
    req: GuiRequest,
) -> GuiReply {
    match req {
        // Hello и Subscribe разбирает транспорт (auth-гейт и смена
        // режима соединения); повторный Hello — idempotentный Ok.
        GuiRequest::Hello { .. } => GuiReply::Ok,
        GuiRequest::Subscribe { .. } => GuiReply::Error {
            message: "Subscribe обрабатывается транспортом сессии".to_string(),
        },
        GuiRequest::SyncProfiles { profiles } => {
            let count = profiles.len();
            daemon
                .registry
                .write()
                .expect("реестр профилей")
                .replace_all(profiles);
            let mut ctx = serde_json::Map::new();
            ctx.insert("count".to_string(), count.into());
            emit(daemon, "profiles.synced", ctx);
            GuiReply::Ok
        }
        GuiRequest::UpsertProfile { profile } => {
            daemon
                .registry
                .write()
                .expect("реестр профилей")
                .upsert(*profile);
            GuiReply::Ok
        }
        GuiRequest::RemoveProfile { id } => {
            // Поднятую бутылку удалять нельзя (как запрет Delete раньше):
            // start держит слушатели и сессию аплаенса. Failed — не
            // активная: namespace разобран при неудаче старта.
            if let Some(manager) = &daemon.manager {
                let active = manager
                    .status()
                    .await
                    .into_iter()
                    .any(|state| state.id == id && crate::pipe::is_active_state(&state.state));
                if active {
                    return GuiReply::Error {
                        message: format!(
                            "бутылка {id} активна — остановите её перед удалением"
                        ),
                    };
                }
            }
            let removed = daemon
                .registry
                .write()
                .expect("реестр профилей")
                .remove(&id);
            if removed {
                GuiReply::Ok
            } else {
                GuiReply::Error {
                    message: format!("бутылка не найдена: {id}"),
                }
            }
        }
        GuiRequest::StartBottle { id } => match &daemon.manager {
            Some(manager) => map_manager(manager.start(&id).await),
            None => GuiReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        GuiRequest::StopBottle { id } => match &daemon.manager {
            Some(manager) => map_manager(manager.stop(&id).await),
            None => GuiReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        GuiRequest::Shutdown => {
            emit(daemon, "daemon.shutdown", serde_json::Map::new());
            if let Some(manager) = &daemon.manager {
                let failed = manager.stop_all().await;
                if !failed.is_empty() {
                    tracing::warn!(ids = ?failed, "часть бутылок не остановилась при shutdown");
                }
            }
            // Отменяем serve с задержкой: кадр ответа должен уйти
            // клиенту до остановки транспорта.
            if let Some(shutdown) = shutdown {
                let token = shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    token.cancel();
                });
            }
            GuiReply::Ok
        }
        GuiRequest::Doctor => match (&daemon.manager, &daemon.wsl) {
            (Some(manager), Some(wsl)) => GuiReply::DoctorReport {
                items: crate::doctor::run(wsl.as_ref(), &daemon.registry, manager).await,
            },
            _ => GuiReply::Error {
                message: MANAGER_NOT_WIRED.to_string(),
            },
        },
        GuiRequest::LogsExport { dest } => {
            let profiles = registry_profiles(daemon);
            match crate::export::export_logs(
                &daemon.data_root,
                &profiles,
                daemon.manager.as_deref(),
                dest,
            )
            .await
            {
                Ok(path) => GuiReply::Path(path.to_string_lossy().into_owned()),
                Err(error) => {
                    tracing::warn!(%error, "экспорт журналов не удался");
                    GuiReply::Error {
                        message: format!("не удалось выгрузить журналы: {error}"),
                    }
                }
            }
        }
        GuiRequest::Stats => GuiReply::Stats(Box::new(collect_stats(daemon).await)),
    }
}

/// Профили реестра простым списком (сводки export/Stats).
fn registry_profiles(daemon: &Daemon) -> Vec<bottle_core::profile::BottleProfile> {
    let registry = daemon.registry.read().expect("реестр профилей");
    registry
        .ids()
        .into_iter()
        .filter_map(|id| registry.get(&id).map(|full| full.profile))
        .collect()
}

/// Ответ менеджера (старый тип CliReply) → GuiReply.
fn map_manager(reply: Result<CliReply, crate::error::DaemonError>) -> GuiReply {
    match reply {
        Ok(CliReply::Started { .. }) | Ok(CliReply::Stopped { .. }) => GuiReply::Ok,
        Ok(CliReply::Error { message }) => GuiReply::Error { message },
        Ok(other) => GuiReply::Error {
            message: format!("неожиданный ответ менеджера: {other:?}"),
        },
        Err(error) => GuiReply::Error {
            message: error.to_string(),
        },
    }
}

/// Статистика демона для карт «Диагностики» (спека §8).
async fn collect_stats(daemon: &Daemon) -> DaemonStats {
    let (mem_bytes, cpu_pct) = daemon.sampler.sample(std::process::id()).await;
    let (active_bottles, appliance_version) = match &daemon.manager {
        Some(manager) => (
            manager
                .status()
                .await
                .iter()
                .filter(|state| crate::pipe::is_active_state(&state.state))
                .count() as u32,
            manager.appliance_version().await,
        ),
        None => (0, None),
    };
    let total_profiles = daemon.registry.read().expect("реестр профилей").len() as u32;
    DaemonStats {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: daemon.started_ts.clone(),
        active_bottles,
        total_profiles,
        mem_bytes,
        cpu_pct,
        wsl_state: wsl_state(daemon).await,
        appliance_version,
    }
}

/// Состояние WSL-VM аплаенса: `running`/`stopped`/`unknown` (без WSL —
/// `unknown`, например на macOS dev-сборке).
#[cfg(windows)]
async fn wsl_state(daemon: &Daemon) -> String {
    let Some(wsl) = &daemon.wsl else {
        return "unknown".to_string();
    };
    match wsl.run(&["-l", "--running", "-q"], None).await {
        Ok(listing) => {
            // Вывод WSL — UTF-16, нормализацию делает runner; сравнение
            // по вхождению имени дистрибуции.
            if listing.contains(crate::appliance::DISTRIBUTION) {
                "running".to_string()
            } else {
                "stopped".to_string()
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

/// На unix WSL нет — карты показывают «unknown».
#[cfg(not(windows))]
async fn wsl_state(_daemon: &Daemon) -> String {
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bottle_core::ipc::FullProfile;
    use bottle_core::profile::{BottleKind, BottleProfile, ExposeConfig, TunnelProtocol, VpnConfig};

    fn full_profile(id: &str) -> FullProfile {
        FullProfile {
            profile: BottleProfile {
                id: id.to_string(),
                name: format!("Бутылка {id}"),
                kind: BottleKind::Env,
                vpn: VpnConfig::Env {
                    protocol: TunnelProtocol::WireGuard,
                    config: format!("{id}.conf"),
                },
                expose: ExposeConfig {
                    socks5: 0,
                    forwards: vec![],
                    dns: None,
                    launch: None,
                },
            },
            config_text: Some("wg".to_string()),
            credentials: None,
        }
    }

    #[tokio::test]
    async fn sync_profiles_replaces_registry_and_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);
        let mut rx = daemon.events.subscribe();

        let reply = gui_dispatch(
            &daemon,
            None,
            GuiRequest::SyncProfiles {
                profiles: vec![full_profile("a"), full_profile("b")],
            },
        )
        .await;
        assert_eq!(reply, GuiReply::Ok);
        assert_eq!(daemon.registry.read().expect("реестр").ids(), ["a", "b"]);

        let bottle_core::ipc::DaemonEvent::Log(event) = rx.recv().await.expect("событие sync") else {
            panic!("ожидали Log-событие");
        };
        assert_eq!(event.event, "profiles.synced");
        assert_eq!(event.level, bottle_core::events::Level::Debug);
        assert!(event.desc.contains('2'), "число профилей: {}", event.desc);

        // Повторный Sync — полная замена, не слияние.
        let reply = gui_dispatch(
            &daemon,
            None,
            GuiRequest::SyncProfiles {
                profiles: vec![full_profile("c")],
            },
        )
        .await;
        assert_eq!(reply, GuiReply::Ok);
        assert_eq!(daemon.registry.read().expect("реестр").ids(), ["c"]);
    }

    #[tokio::test]
    async fn upsert_and_remove_profile_are_pointwise() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);

        gui_dispatch(
            &daemon,
            None,
            GuiRequest::SyncProfiles {
                profiles: vec![full_profile("a")],
            },
        )
        .await;
        let reply = gui_dispatch(
            &daemon,
            None,
            GuiRequest::UpsertProfile {
                profile: Box::new(full_profile("b")),
            },
        )
        .await;
        assert_eq!(reply, GuiReply::Ok);
        assert_eq!(daemon.registry.read().expect("реестр").ids(), ["a", "b"]);

        let reply = gui_dispatch(
            &daemon,
            None,
            GuiRequest::RemoveProfile { id: "a".into() },
        )
        .await;
        assert_eq!(reply, GuiReply::Ok);
        assert_eq!(daemon.registry.read().expect("реестр").ids(), ["b"]);

        // Удаление отсутствующего — понятная ошибка.
        let reply = gui_dispatch(
            &daemon,
            None,
            GuiRequest::RemoveProfile { id: "ghost".into() },
        )
        .await;
        let GuiReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert!(message.contains("ghost"), "{message}");
    }

    #[tokio::test]
    async fn start_without_manager_is_transitional_error() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);
        let reply = gui_dispatch(&daemon, None, GuiRequest::StartBottle { id: "a".into() }).await;
        let GuiReply::Error { message } = reply else {
            panic!("ожидали Error, получили {reply:?}");
        };
        assert_eq!(message, MANAGER_NOT_WIRED);
    }

    #[tokio::test]
    async fn stats_reports_shape_without_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);
        let reply = gui_dispatch(&daemon, None, GuiRequest::Stats).await;
        let GuiReply::Stats(stats) = reply else {
            panic!("ожидали Stats, получили {reply:?}");
        };
        assert_eq!(stats.pid, std::process::id());
        assert_eq!(stats.total_profiles, 0);
        assert_eq!(stats.wsl_state, "unknown");
        assert!(stats.mem_bytes > 0, "RSS собственного процесса известен");
    }

    #[tokio::test]
    async fn shutdown_cancels_token_after_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = crate::test_daemon(tmp.path(), "tok", None);
        let shutdown = CancellationToken::new();
        let reply = gui_dispatch(&daemon, Some(&shutdown), GuiRequest::Shutdown).await;
        assert_eq!(reply, GuiReply::Ok);
        assert!(!shutdown.is_cancelled(), "отмена отложена до ответа");
        tokio::time::timeout(std::time::Duration::from_secs(2), shutdown.cancelled())
            .await
            .expect("отмена наступила");
    }
}
