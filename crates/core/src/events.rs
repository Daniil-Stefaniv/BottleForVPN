//! Реестр событий логирования (спека §6): каждая запись журнала — событие
//! с кодом из реестра, уровнем, источником, описанием по шаблону и
//! структурным контекстом. Единый формат JSONL для daemon/gui/svc.

use serde::{Deserialize, Serialize};

/// Уровень события; serde — lowercase на проводе и в JSONL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// Источник события; serde — lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Src {
    Daemon,
    Gui,
    Svc,
}

/// Запись журнала: одна строка JSONL с полями
/// `ts,level,src,event,desc,ctx` (спека §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// RFC3339 UTC с миллисекундами (`2026-10-21T21:12:04.331Z`).
    pub ts: String,
    pub level: Level,
    pub src: Src,
    /// Код события из [`REGISTRY`]. `String`, а не `&'static str`:
    /// событие ездит по каналу GUI↔демон и десериализуется на приёме.
    pub event: String,
    /// Описание по шаблону реестра с подставленными параметрами.
    pub desc: String,
    /// Структурные параметры; секреты запрещены (спека §6).
    #[serde(default)]
    pub ctx: serde_json::Map<String, serde_json::Value>,
}

/// Определение события в реестре: код, уровень по умолчанию, шаблон
/// описания с плейсхолдерами `{name}`.
#[derive(Debug, Clone, Copy)]
pub struct EventDef {
    pub code: &'static str,
    pub level: Level,
    pub template: &'static str,
}

/// Ядро реестра событий (спека §6). Шаблоны подставляют параметры из
/// `ctx` по плейсхолдерам `{name}`; секреты в шаблонах запрещены.
pub const REGISTRY: &[EventDef] = &[
    EventDef { code: "db.unlocked", level: Level::Info, template: "Хранилище разблокировано" },
    EventDef { code: "db.unlock_failed", level: Level::Warn, template: "Неверный мастер-пароль (попытка {n})" },
    EventDef { code: "db.rekeyed", level: Level::Info, template: "Мастер-пароль изменён" },
    EventDef { code: "db.corrupt", level: Level::Fatal, template: "Хранилище повреждено: {detail}" },
    EventDef { code: "daemon.spawned", level: Level::Info, template: "Демон запущен (pid {pid})" },
    EventDef { code: "daemon.exited", level: Level::Error, template: "Демон завершился неожиданно (код {code})" },
    EventDef { code: "daemon.shutdown", level: Level::Info, template: "Демон остановлен по команде пользователя" },
    EventDef { code: "profiles.synced", level: Level::Debug, template: "Демону передано профилей: {count}" },
    EventDef { code: "bottle.created", level: Level::Info, template: "Бутылка {id} создана" },
    EventDef { code: "bottle.updated", level: Level::Info, template: "Бутылка {id} изменена" },
    EventDef { code: "bottle.deleted", level: Level::Info, template: "Бутылка {id} удалена" },
    EventDef { code: "bottle.credentials_updated", level: Level::Info, template: "Обновлены логин и пароль бутылки {id}" },
    EventDef { code: "bottle.start.requested", level: Level::Info, template: "Пользователь запустил бутылку {id}" },
    EventDef { code: "bottle.up", level: Level::Info, template: "{id} поднята (socks5 :{port})" },
    EventDef { code: "bottle.failed", level: Level::Error, template: "{id}: {reason}" },
    EventDef { code: "wsl.imported", level: Level::Info, template: "Апплайенс импортирован" },
    EventDef { code: "svc.route_added", level: Level::Debug, template: "Маршрут {cidr} добавлен" },
];

/// Рендерит событие по коду: уровень из реестра и описание с
/// подставленными из `ctx` значениями. `None` — код не из реестра.
/// Подстановка — простая замена `{name}` строковым представлением
/// значения (строки без кавычек, остальное — компактный JSON).
pub fn render(code: &str, ctx: serde_json::Map<String, serde_json::Value>) -> Option<(Level, String)> {
    let def = REGISTRY.iter().find(|d| d.code == code)?;
    let mut desc = def.template.to_string();
    for (key, value) in &ctx {
        let placeholder = format!("{{{key}}}");
        if desc.contains(&placeholder) {
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            desc = desc.replace(&placeholder, &text);
        }
    }
    Some((def.level, desc))
}

/// Текущая метка времени: RFC3339 UTC с миллисекундами
/// (`2026-10-21T21:12:04.331Z`).
pub fn now_ts() -> String {
    const FORMAT: &[time::format_description::FormatItem<'static>] =
        time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
    time::OffsetDateTime::now_utc()
        .format(FORMAT)
        .expect("дата форматируется")
}

/// Оставляет в `ctx` только ключи, которые шаблон события реально
/// подставляет (`{key}`), и урезает строковые значения до 200 символов.
/// Код вне реестра → пустой контекст. Гарантия «секреты не в журнале»
/// (спека §6): вызывающий может передать широкий ctx, в запись уйдут
/// только публичные параметры шаблона.
pub fn sanitize_ctx(
    code: &str,
    ctx: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let Some(def) = REGISTRY.iter().find(|d| d.code == code) else {
        return serde_json::Map::new();
    };
    let mut out = serde_json::Map::new();
    for (key, mut value) in ctx {
        if !def.template.contains(&format!("{{{key}}}")) {
            continue;
        }
        if let serde_json::Value::String(text) = &mut value {
            const MAX: usize = 200;
            if text.chars().count() > MAX {
                *text = text.chars().take(MAX).collect();
            }
        }
        out.insert(key, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_codes_are_unique_and_render() {
        let mut seen = std::collections::HashSet::new();
        for def in REGISTRY {
            assert!(seen.insert(def.code), "дубль кода {}", def.code);
            assert!(def.code.contains('.'), "код без категории: {}", def.code);
        }
        let mut ctx = serde_json::Map::new();
        ctx.insert("n".into(), 3.into());
        let (level, desc) = render("db.unlock_failed", ctx).unwrap();
        assert_eq!(level, Level::Warn);
        assert_eq!(desc, "Неверный мастер-пароль (попытка 3)");
    }

    #[test]
    fn event_serializes_as_jsonl_line() {
        let e = Event {
            ts: now_ts(),
            level: Level::Info,
            src: Src::Gui,
            event: "bottle.created".to_string(),
            desc: "Бутылка a создана".into(),
            ctx: Default::default(),
        };
        let line = serde_json::to_string(&e).unwrap();
        assert!(line.contains(r#""src":"gui""#) && line.contains(r#""event":"bottle.created""#));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn sanitize_ctx_keeps_only_template_keys() {
        let mut ctx = serde_json::Map::new();
        ctx.insert("id".into(), "office-a".into());
        ctx.insert("port".into(), 1080.into());
        ctx.insert("password".into(), "s3cr3t".into());
        ctx.insert("PrivateKey".into(), "ABCDEF".into());
        let out = sanitize_ctx("bottle.up", ctx);
        assert!(out.contains_key("id") && out.contains_key("port"));
        assert!(!out.contains_key("password") && !out.contains_key("PrivateKey"));

        // Код вне реестра — контекст пуст.
        let mut ctx = serde_json::Map::new();
        ctx.insert("id".into(), "x".into());
        assert!(sanitize_ctx("nope", ctx).is_empty());

        // Длинные строки урезаются.
        let mut ctx = serde_json::Map::new();
        ctx.insert("reason".into(), "я".repeat(500).into());
        let out = sanitize_ctx("bottle.failed", ctx);
        let reason = out["reason"].as_str().unwrap();
        assert_eq!(reason.chars().count(), 200);
    }

    #[test]
    fn now_ts_is_rfc3339_utc_millis() {
        let ts = now_ts();
        // 2026-10-21T21:12:04.331Z — 24 символа, Z на конце.
        assert_eq!(ts.len(), 24, "ts: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
    }
}
