//! Tray-меню GUI: «Показать окно», до 8 бутылок со старт/стопом
//! (+ «…» если больше), «Завершить» — полный выход (0.3): GUI эмитит
//! `quit-requested`, фронтенд спрашивает подтверждение и вызывает
//! `confirm_quit` (Shutdown демону → exit). Список обновляется из TS
//! командой `update_tray`, действия меню возвращаются ивентом
//! `tray-action`.

use serde::{Deserialize, Serialize};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Максимум бутылок в tray-меню; хвост схлопывается в «…».
const MAX_TRAY_BOTTLES: usize = 8;

/// Запись о бутылке для tray-меню (приходит из TS после каждого refresh).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayEntry {
    /// Идентификатор бутылки (для старта/останова).
    pub id: String,
    /// Подпись пункта (имя + состояние, готовит TS).
    pub label: String,
    /// Работает ли бутылка (маркер ● в меню).
    pub running: bool,
}

/// Действие tray-меню, уходящее фронтенду ивентом `tray-action`.
/// Show/More/Exit обрабатываются в Rust (окно/выход) и сюда не входят.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrayAction {
    /// Старт бутылки по id.
    Start { id: String },
    /// Стоп бутылки по id.
    Stop { id: String },
}

/// Режет список бутылок до лимита меню; `more` — был ли скрытый хвост.
pub fn truncate_tray_entries(entries: Vec<TrayEntry>) -> (Vec<TrayEntry>, bool) {
    let more = entries.len() > MAX_TRAY_BOTTLES;
    if more {
        (entries.into_iter().take(MAX_TRAY_BOTTLES).collect(), true)
    } else {
        (entries, false)
    }
}

/// Создаёт tray-иконку с базовым меню (вызывается один раз в setup).
pub fn create_tray<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = build_menu(app, &[], false)?;
    tauri::tray::TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon()
                .expect("иконка приложения")
                .clone(),
        )
        .tooltip("Погребок")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            emit_menu_action(app, event.id().as_ref());
        })
        .build(app)?;
    Ok(())
}

/// Перестраивает меню tray по списку бутылок (команда update_tray).
pub fn update_tray_menu<R: Runtime>(
    app: &AppHandle<R>,
    entries: Vec<TrayEntry>,
) -> tauri::Result<()> {
    let (shown, more) = truncate_tray_entries(entries);
    let menu = build_menu(app, &shown, more)?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu))?;
    }
    Ok(())
}

/// Собирает меню: «Показать окно» · бутылки (старт/стоп) · «…» · «Выход».
fn build_menu<R: Runtime>(
    app: &AppHandle<R>,
    entries: &[TrayEntry],
    more: bool,
) -> tauri::Result<tauri::menu::Menu<R>> {
    let show = MenuItemBuilder::with_id("show", "Показать окно").build(app)?;
    let mut builder = MenuBuilder::new(app).item(&show);
    if !entries.is_empty() || more {
        builder = builder.separator();
    }
    for entry in entries {
        let title = if entry.running {
            format!("● {}", entry.label)
        } else {
            format!("○ {}", entry.label)
        };
        let submenu = SubmenuBuilder::new(app, &title)
            .text(format!("start:{}", entry.id), "Старт")
            .text(format!("stop:{}", entry.id), "Стоп")
            .build()?;
        builder = builder.item(&submenu);
    }
    if more {
        builder = builder.item(&MenuItemBuilder::with_id("more", "…").build(app)?);
    }
    builder = builder
        .separator()
        .item(&MenuItemBuilder::with_id("exit", "Завершить").build(app)?);
    // Заглушка раздела «Правка» macOS не нужна: PredefinedMenuItem не
    // используем кроме как для корректных сепараторов.
    let _ = PredefinedMenuItem::separator(app);
    builder.build()
}

/// Превращает id пункта меню в действие: show — окно показывается здесь
/// же (без прав у TS); exit — ивент `quit-requested` фронтенду (0.3:
/// подтверждение и Shutdown демона — там); прочее — ивент `tray-action`.
fn emit_menu_action<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "exit" => {
            if let Err(error) = app.emit("quit-requested", ()) {
                eprintln!("не удалось отправить quit-requested: {error}");
            }
        }
        _ => {
            let action = match id.split_once(':') {
                Some(("start", bottle)) => TrayAction::Start {
                    id: bottle.to_string(),
                },
                Some(("stop", bottle)) => TrayAction::Stop {
                    id: bottle.to_string(),
                },
                _ => return,
            };
            if let Err(error) = app.emit("tray-action", &action) {
                eprintln!("не удалось отправить событие tray: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, running: bool) -> TrayEntry {
        TrayEntry {
            id: id.to_string(),
            label: format!("Бутылка {id}"),
            running,
        }
    }

    #[test]
    fn truncate_keeps_first_eight_and_marks_overflow() {
        let entries: Vec<TrayEntry> = (0..10).map(|i| entry(&format!("b{i}"), false)).collect();
        let (shown, more) = truncate_tray_entries(entries);
        assert_eq!(shown.len(), 8, "максимум 8 бутылок в меню");
        assert_eq!(shown[0].id, "b0");
        assert_eq!(shown[7].id, "b7");
        assert!(more, "10 бутылок — есть скрытый хвост");
    }

    #[test]
    fn exactly_eight_is_not_overflow() {
        let entries: Vec<TrayEntry> = (0..8).map(|i| entry(&format!("b{i}"), false)).collect();
        let (shown, more) = truncate_tray_entries(entries);
        assert_eq!(shown.len(), 8);
        assert!(!more);
    }

    #[test]
    fn fewer_than_eight_passes_through() {
        let entries = vec![entry("a", true), entry("b", false)];
        let (shown, more) = truncate_tray_entries(entries);
        assert_eq!(shown.len(), 2);
        assert!(!more);
    }

    #[test]
    fn empty_list_is_empty_not_overflow() {
        let (shown, more) = truncate_tray_entries(vec![]);
        assert!(shown.is_empty());
        assert!(!more);
    }
}
