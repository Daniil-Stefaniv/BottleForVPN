//! Журнал GUI (0.3, спека §6): действия пользователя и события окна в
//! `gui-YYYYMMDD.jsonl` (тот же формат JSONL, что у демона) плюс
//! кольцевой буфер последних [`RING_LIMIT`] событий для живой консоли
//! «Диагностики» (слияние лент daemon/gui — Task 17).
//!
//! Коды событий — только из реестра [`events::REGISTRY`] (единый
//! источник): неизвестный код — ошибка вызывающему, строка не пишется.
//! Контекст пропускается через [`events::sanitize_ctx`] — в журнал не
//! уйдут ключи, которых нет в шаблоне (секреты).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bottle_core::events::{self, Event, Src};

/// Ёмкость кольцевого буфера консоли (свежие события вытесняют старые).
const RING_LIMIT: usize = 5000;

struct Inner {
    /// Писатель в JSONL-файл; `None` — degraded-режим (файл не открылся,
    /// журнал живёт только в буфере консоли).
    writer: Option<BufWriter<File>>,
    /// Кольцевой буфер для `recent()`.
    ring: VecDeque<Event>,
}

/// Журнал GUI: JSONL-файл + кольцевой буфер. Клонируемый не нужен —
/// живёт в tauri State.
pub struct GuiLog {
    inner: Mutex<Inner>,
}

impl GuiLog {
    /// Открывает журнал: `logs_dir/gui-YYYYMMDD.jsonl` (дописываем,
    /// каталог создаётся). Дата файла — день старта приложения.
    pub fn init(logs_dir: &Path) -> Result<GuiLog, String> {
        std::fs::create_dir_all(logs_dir)
            .map_err(|e| format!("не удалось создать каталог журналов: {e}"))?;
        let date = &events::now_ts()[..10].replace('-', "");
        let path = logs_dir.join(format!("gui-{date}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
        Ok(Self::with_writer(Some(BufWriter::new(file))))
    }

    /// Журнал только в памяти (файл недоступен — GUI продолжает
    /// работать, консоль «Диагностики» показывает события сессии).
    pub fn memory_only() -> GuiLog {
        Self::with_writer(None)
    }

    fn with_writer(writer: Option<BufWriter<File>>) -> GuiLog {
        GuiLog {
            inner: Mutex::new(Inner {
                writer,
                ring: VecDeque::new(),
            }),
        }
    }

    /// Пишет событие реестра: уровень и описание — по шаблону кода,
    /// `src` всегда `gui`. Неизвестный код — ошибка, строка не пишется.
    pub fn emit(
        &self,
        code: &str,
        ctx: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        let (level, desc) = events::render(code, ctx.clone())
            .ok_or_else(|| format!("неизвестный код события: {code}"))?;
        let event = Event {
            ts: events::now_ts(),
            level,
            src: Src::Gui,
            event: code.to_string(),
            desc,
            ctx: events::sanitize_ctx(code, ctx),
        };
        let line =
            serde_json::to_string(&event).map_err(|e| format!("событие не сериализуется: {e}"))?;

        let mut inner = self.inner.lock().expect("мьютекс журнала не отравлен");
        if let Some(writer) = inner.writer.as_mut() {
            // Ошибка записи не роняет GUI: журнал деградирует до буфера.
            if writeln!(writer, "{line}")
                .and_then(|()| writer.flush())
                .is_err()
            {
                inner.writer = None;
            }
        }
        inner.ring.push_back(event);
        if inner.ring.len() > RING_LIMIT {
            inner.ring.pop_front();
        }
        Ok(())
    }

    /// Последние события буфера (для слияния лент в консоли).
    pub fn recent(&self) -> Vec<Event> {
        self.inner
            .lock()
            .expect("мьютекс журнала не отравлен")
            .ring
            .iter()
            .cloned()
            .collect()
    }

    /// Путь к файлу журнала (для «открыть папку логов»): каталог.
    pub fn logs_dir() -> Option<PathBuf> {
        bottle_core::ipc::default_data_dir().map(|dir| dir.join("logs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// emit пишет JSONL-строку с src:"gui" и кодом; recent() отдаёт её.
    #[test]
    fn emit_writes_jsonl_and_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let log = GuiLog::init(dir.path()).unwrap();

        log.emit("bottle.created", ctx(&[("id", "office-a".into())]))
            .unwrap();
        log.emit("daemon.spawned", ctx(&[("pid", 4217.into())]))
            .unwrap();

        let file = dir
            .path()
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("gui-"));
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains(r#""src":"gui""#), "{}", lines[0]);
        assert!(
            lines[0].contains(r#""event":"bottle.created""#),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("Бутылка office-a создана"),
            "{}",
            lines[0]
        );

        let recent = log.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].event, "bottle.created");
        assert_eq!(recent[1].desc, "Демон запущен (pid 4217)");
    }

    /// Неизвестный код — ошибка, строка в журнал не пишется (реестр —
    /// единый источник).
    #[test]
    fn unknown_code_is_error_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = GuiLog::init(dir.path()).unwrap();

        let error = log.emit("hack.the.planet", Default::default()).unwrap_err();
        assert!(error.contains("неизвестный код"), "{error}");
        assert!(log.recent().is_empty());
        let text = std::fs::read_to_string(
            dir.path()
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(text.is_empty(), "{text}");
    }

    /// Буфер не растёт за RING_LIMIT: свежие вытесняют старые.
    #[test]
    fn ring_buffer_caps_at_limit() {
        let log = GuiLog::memory_only();
        for n in 0..RING_LIMIT + 100 {
            log.emit("db.unlock_failed", ctx(&[("n", n.into())]))
                .unwrap();
        }
        let recent = log.recent();
        assert_eq!(recent.len(), RING_LIMIT);
        // Последнее событие — самое свежее (n = limit+99).
        let expected_last = serde_json::Value::from((RING_LIMIT + 99) as u64);
        assert_eq!(recent.last().unwrap().ctx["n"], expected_last);
        // Первое в буфере — n=100 (сто первых вытеснены).
        assert_eq!(recent[0].ctx["n"], serde_json::Value::from(100u64));
    }

    /// memory_only: emit работает без файла (degraded-режим).
    #[test]
    fn memory_only_still_buffers() {
        let log = GuiLog::memory_only();
        log.emit("daemon.shutdown", Default::default()).unwrap();
        assert_eq!(log.recent().len(), 1);
    }
}
