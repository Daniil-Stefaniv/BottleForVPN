//! UDS-транспорт демона (macOS/Unix, M4 Task 7): сокет `<data>/daemon.sock`,
//! права 0600 (Review Focus #2: каталог данных уже 0700 у Store — чужой
//! пользователь машины не подключится), протокол — те же кадры
//! LengthJsonCodec и тот же диспетчер [`crate::pipe::dispatch`], что у
//! named pipe Windows. Принятое соединение живёт в своей задаче —
//! параллельные CLI не ждут друг друга (T13).

use std::io;
use std::sync::Arc;

use bottle_core::frame::LengthJsonCodec;
use bottle_core::ipc::{CliReply, CliRequest};
use bytes::BytesMut;
use tokio::io::{split, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tokio_util::codec::{Encoder, FramedRead};
use tokio_util::sync::CancellationToken;

use crate::Daemon;

/// Обслуживает UDS-сокет `socket_path` до отмены `shutdown`.
///
/// Мёртвый файл сокета от прошлого запуска сносится до bind; живой
/// демон (connect-проба отвечает) защищает свой сокет — второй serve
/// отказывает, а не плодит два демона над одним хранилищем. При
/// остановке файл убирается только если он по-прежнему наш (inode),
/// чтобы не снести сокет демона, перебиндившего путь за наше время.
pub async fn serve(
    socket_path: &std::path::Path,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    // Хвост от упавшего демона: bind на занятый путь невозможен.
    if socket_path.exists() {
        if socket_is_live(socket_path).await {
            return Err(io::Error::other(
                "демон уже запущен: сокет отвечает (см. каталог данных)",
            ));
        }
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    set_owner_only(socket_path)?;
    let owned_ino = file_inode(socket_path);
    tracing::info!(socket = %socket_path.display(), "uds-сервер запущен");

    let mut connections: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let connection =
                    handle_connection(stream, Arc::clone(&daemon), shutdown.clone());
                connections.spawn(connection);
            }
            () = shutdown.cancelled() => {
                tracing::info!("uds-сервер остановлен по отмене");
                connections.abort_all();
                while connections.join_next().await.is_some() {
                    // Ждём фактического завершения задач.
                }
                // Файл сокета убираем сами — но только наш (по inode):
                // путь мог перехватить другой демон за время остановки.
                if file_inode(socket_path) == owned_ino {
                    let _ = std::fs::remove_file(socket_path);
                }
                return Ok(());
            }
        }
    }
}

/// Отвечает ли по этому пути живой слушатель (connect-проба).
async fn socket_is_live(socket_path: &std::path::Path) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

/// inode файла сокета (для защиты «удаляем только свой»), либо None.
fn file_inode(socket_path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(socket_path).ok().map(|meta| meta.ino())
}

/// Права сокета: 0600 — владелец (создатель демона) читает/пишет.
fn set_owner_only(socket_path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
}

/// Обслуживает одно подключение CLI: кадры запросов → ответы диспетчера
/// (тот же цикл, что у named pipe — включая отмену исполнения по
/// shutdown посреди долгих команд).
async fn handle_connection(
    stream: tokio::net::UnixStream,
    daemon: Arc<Daemon>,
    shutdown: CancellationToken,
) {
    let (read_half, mut write_half) = split(stream);
    let mut reader = FramedRead::new(read_half, LengthJsonCodec::<CliRequest>::new());
    let mut reply_codec = LengthJsonCodec::<CliReply>::new();

    loop {
        let req = tokio::select! {
            maybe = reader.next() => match maybe {
                Some(Ok(req)) => req,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "некорректный кадр от CLI, соединение закрывается");
                    break;
                }
                None => break,
            },
            () = shutdown.cancelled() => break,
        };

        let reply = tokio::select! {
            reply = crate::pipe::dispatch(
                &daemon.store,
                daemon.manager.as_deref(),
                daemon.wsl.as_deref(),
                daemon.rootfs.as_deref(),
                Some(&shutdown),
                req,
            ) => reply,
            () = shutdown.cancelled() => break,
        };
        if let Err(e) = send_reply(&mut write_half, &mut reply_codec, reply).await {
            tracing::warn!(error = %e, "не удалось отправить ответ CLI");
            break;
        }
    }
}

/// Кодирует и отправляет ответ CLI (fallible-путь `Encoder`).
async fn send_reply<W>(
    write_half: &mut W,
    codec: &mut LengthJsonCodec<CliReply>,
    reply: CliReply,
) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    codec.encode(reply, &mut buf).map_err(io::Error::other)?;
    write_half.write_all(&buf).await
}
