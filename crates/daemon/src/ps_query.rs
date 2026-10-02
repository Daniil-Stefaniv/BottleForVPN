//! Читающие PowerShell-запросы из демона (M2 T8): `Get-NetRoute` /
//! `Get-NetAdapter` для поиска физического шлюза и снапшотов
//! адаптеров. Демон НЕ запускает PowerShell для изменения маршрутов —
//! только для чтения сети; изменяющие операции — у службы `bottle-svc`.
//!
//! `encode_ps` и `ps_value` — ЛОКАЛЬНЫЕ КОПИИ из
//! `crates/svc/src/routes.rs`: `svc` — bin-крейт, его библиотека
//! недоступна демону; дублирование осознанное (прецедент — hex-хелпер
//! из ledger M1), корректность зафиксирована эталонами тестов.

use std::io;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Таймаут читающего запроса: зависший PowerShell снимается, демон
/// не блокируется навсегда (спека службы — до 30 с на командлет;
/// читающие запросы быстрее).
const PS_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Пауза опроса завершения PowerShell в блокирующем исполнителе.
const WAIT_STEP: Duration = Duration::from_millis(50);

/// Значение-аргумент командлета в PS-одинарных кавычках. Внутри них
/// `$`, `;`, backtick, `|`, скобки — литералы; единственное
/// экранирование — удвоение самой одинарной кавычки. Флаги и имена
/// командлетов НЕ квотируются (копия `ps_value` из svc/routes.rs).
pub(crate) fn ps_value(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// Кодирует аргументы в значение параметра `-EncodedCommand`
/// PowerShell: аргументы, соединённые пробелами в строку скрипта →
/// UTF-16LE → base64 (копия `encode_ps_command` из svc/routes.rs).
pub(crate) fn encode_ps(args: &[String]) -> String {
    let script = args.join(" ");
    let mut utf16le = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16le.extend_from_slice(&unit.to_le_bytes());
    }
    base64_encode(&utf16le)
}

/// Кодирует байты в стандартный base64 (RFC 4648, базовый алфавит,
/// с паддингом). Копия из svc/routes.rs; эталоны RFC 4648 — в тестах.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(triple[0]) << 16) | (u32::from(triple[1]) << 8) | u32::from(triple[2]);
        out.push(ALPHABET[(n >> 18) as usize & 0b111111] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0b111111] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0b111111] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0b111111] as char
        } else {
            '='
        });
    }
    out
}

/// Запускает читающий PowerShell-запрос и возвращает stdout.
///
/// `spawn_blocking` + `std::process::Command` (паттерн
/// `appliance::RealWsl`): `powershell -NoProfile -NonInteractive
/// -EncodedCommand <encode>` — сценарий передаётся одним base64-
/// токеном (UTF-16LE), разбор не зависит от кавычек/метасимволов
/// оболочки. Зависший PowerShell снимается по [`PS_READ_TIMEOUT`].
/// Ненулевой код выхода — ошибка со stderr.
pub(crate) async fn run_ps_readonly(args: &[String]) -> io::Result<String> {
    let encoded = encode_ps(args);
    tokio::task::spawn_blocking(move || run_ps_sync(&encoded))
        .await
        .map_err(|e| io::Error::other(format!("задача powershell отменена: {e}")))?
}

/// Блокирующий исполнитель одного `-EncodedCommand`-запроса.
///
/// Вывод читается после завершения процесса: читающие запросы
/// (`Get-NetRoute`/`Get-NetAdapter`) умещаются в буфер пайпа, дедлока
/// на заполненном stdout нет.
fn run_ps_sync(encoded: &str) -> io::Result<String> {
    let mut child = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-EncodedCommand")
        .arg(encoded)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::other(format!("не удалось запустить powershell: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout piped выше");
    let mut stderr = child.stderr.take().expect("stderr piped выше");

    let deadline = Instant::now() + PS_READ_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(format!(
                    "PowerShell не завершился вовремя (таймаут {} с)",
                    PS_READ_TIMEOUT.as_secs()
                )));
            }
            None => std::thread::sleep(WAIT_STEP),
        }
    };

    let mut out_bytes = Vec::new();
    stdout.read_to_end(&mut out_bytes)?;
    let mut err_bytes = Vec::new();
    stderr.read_to_end(&mut err_bytes)?;

    if status.success() {
        Ok(String::from_utf8_lossy(&out_bytes).into_owned())
    } else {
        Err(io::Error::other(format!(
            "PowerShell завершился с ошибкой: {}",
            String::from_utf8_lossy(&err_bytes).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Пин инъекционной дисциплины: значения — только PS-литералы,
    /// одинарная кавычка внутри удваивается (копия теста svc/routes.rs).
    #[test]
    fn ps_value_quotes_and_doubles_single_quotes() {
        assert_eq!(ps_value("0.0.0.0/0"), "'0.0.0.0/0'");
        assert_eq!(ps_value("a'b"), "'a''b'");
        assert_eq!(ps_value("Ethernet|cmd; rm"), "'Ethernet|cmd; rm'");
    }

    /// Эталон UTF-16LE+base64, посчитан независимо (см. svc/routes.rs).
    #[test]
    fn encode_ps_known_vector_utf16le_base64() {
        assert_eq!(
            encode_ps(&strs(&["Write-Output", "привет"])),
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIAA/BEAEOAQyBDUEQgQ="
        );
    }

    /// Эталон командной строки чтения маршрутов (см. svc/routes.rs).
    #[test]
    fn encode_ps_route_line_vector() {
        assert_eq!(
            encode_ps(&strs(&[
                "Get-NetRoute",
                "-DestinationPrefix",
                "203.0.113.77/32"
            ])),
            concat!(
                "RwBlAHQALQBOAGUAdABSAG8AdQB0AGUAIAAtAEQAZQBzAHQAaQBuAGEAdABpAG8A",
                "bgBQAHIAZQBmAGkAeAAgADIAMAAzAC4AMAAuADEAMQAzAC4ANwA3AC8AMwAyAA=="
            )
        );
    }

    /// Эталоны RFC 4648 §10 для локальной копии base64.
    #[test]
    fn base64_matches_rfc4648_test_vectors() {
        for (text, expected) in [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(text.as_bytes()), expected, "вектор {text:?}");
        }
    }
}
