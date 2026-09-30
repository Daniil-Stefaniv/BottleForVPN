//! Копирование байтов между двумя TCP-соединениями.

use std::io;

use tokio::net::TcpStream;

/// Копирует байты между `a` и `b` в обе стороны до закрытия соединений.
///
/// Тонкая обёртка над [`tokio::io::copy_bidirectional`]: семантика
/// полузакрытия наследуется от tokio — EOF в одну сторону не обрывает
/// встречный поток, пока тот не закроется сам.
pub async fn copy_bidirectional(mut a: TcpStream, mut b: TcpStream) -> io::Result<()> {
    let (a_to_b, b_to_a) = tokio::io::copy_bidirectional(&mut a, &mut b).await?;
    tracing::trace!(a_to_b, b_to_a, "bidirectional copy finished");
    Ok(())
}
