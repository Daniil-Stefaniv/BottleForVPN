//! Чистый рендер ответов демона в пользовательский вывод на русском.
//!
//! В 0.3 CLI — только служебные команды (Task 19), поэтому здесь остался
//! рендер `doctor`; списки/профили бутылок показывает приложение
//! «Погребок». Функции не знают о транспорте и работают на любой
//! платформе — вывод CLI покрыт unit-тестами и на не-Windows хостах.

/// Предупреждение при выгрузке журналов (спека §10): архив собирается
/// локально, но содержит сведения о сети — просмотреть перед отправкой.
pub const LOGS_EXPORT_WARNING: &str = "Внимание: архив содержит сведения о сети \
     (IP-адреса, имена интерфейсов, endpoint'ы VPN) — просмотрите содержимое \
     перед отправкой кому-либо";

/// Рендерит отчёт `doctor`: строки «✓/⚠/✗  код  пояснение» колонками;
/// код выровнен по самому длинному.
pub fn render_doctor(items: &[bottle_core::ipc::CheckItem]) -> String {
    use bottle_core::ipc::CheckStatus;

    if items.is_empty() {
        return "Проверок нет".to_string();
    }
    let glyph = |status: &CheckStatus| match status {
        CheckStatus::Ok => "✓",
        CheckStatus::Warn => "⚠",
        CheckStatus::Fail => "✗",
    };
    let code_width = items
        .iter()
        .map(|item| item.code.chars().count())
        .max()
        .unwrap_or(0)
        .max("код".len());
    let mut out = String::new();
    for item in items {
        out.push_str(&format!(
            "{} {:<code_width$}  {}\n",
            glyph(&item.status),
            item.code,
            item.detail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bottle_core::ipc::{CheckItem, CheckStatus};

    #[test]
    fn doctor_renders_all_status_glyphs_aligned() {
        let items = vec![
            CheckItem {
                code: "wsl".to_string(),
                status: CheckStatus::Ok,
                detail: "WSL включён".to_string(),
            },
            CheckItem {
                code: "service".to_string(),
                status: CheckStatus::Warn,
                detail: "не установлена".to_string(),
            },
            CheckItem {
                code: "agent".to_string(),
                status: CheckStatus::Fail,
                detail: "не отвечает".to_string(),
            },
        ];
        let out = render_doctor(&items);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 3, "по строке на проверку:\n{out}");
        assert!(rows[0].starts_with('✓'), "ok-глиф:\n{out}");
        assert!(rows[1].starts_with('⚠'), "warn-глиф:\n{out}");
        assert!(rows[2].starts_with('✗'), "fail-глиф:\n{out}");
        // Коды выровнены: пояснения начинаются в одной колонке.
        assert_eq!(
            rows[0].find("WSL включён"),
            rows[1].find("не установлена"),
            "колонка пояснений не выровнена:\n{out}"
        );
    }

    #[test]
    fn doctor_without_items_has_russian_message() {
        assert_eq!(render_doctor(&[]), "Проверок нет");
    }
}
