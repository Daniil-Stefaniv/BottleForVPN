//! Разбор и запуск команд из профиля бутылки (T13).
//!
//! Поле `expose.launch` (env- и host-бутылки) и аргументы host-клиента
//! (`[vpn] args`) — это командная строка «программа и её аргументы»,
//! а не shell-скрипт: [`split_command_line`] разбивает её в argv
//! мини-shlex'ом (кавычки, экранированные пробелы), [`spawn_launch`]
//! запускает результат без посредника shell.

/// Разбивает командную строку на аргументы (мини-shlex).
///
/// Поддерживает одинарные и двойные кавычки — содержимое в кавычках не
/// делится по пробелам (`"C:\Program Files\app.exe"` — один аргумент,
/// слэши пути сохраняются) — и экранирование обратным слэшем: `\ ` —
/// пробел вне кавычек, `\"`, `\\` — соответствующий символ. Перед
/// «обычными» символами слэш literal (пути Windows не требуют двойного
/// экранирования; семантика двойных кавычек POSIX). В одинарных кавычках
/// не экранируется ничего. Кавычки склеивают соседние фрагменты
/// (`a"b c"d` → один аргумент `ab cd`), пара кавычек создаёт пустой
/// аргумент. Незакрытая кавычка поглощает остаток строки (мягкое
/// поведение для рукописного поля профиля). Пустая строка и строка
/// из пробелов дают пустой список.
pub(crate) fn split_command_line(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    // Аргумент начат: кавычки считаются началом (`''` — осмысленный
    // пустой аргумент), голый пробел — завершением.
    let mut started = false;
    let mut chars = line.chars();

    /// Экранируемый символ: слэш перед ним теряет спецсмысл.
    fn escapable(c: char) -> bool {
        matches!(c, '"' | '\'' | '\\' | ' ' | '\t' | '\r' | '\n')
    }

    loop {
        let Some(c) = chars.next() else {
            // Строка кончилась: незавершённый аргумент уходит в parts.
            if started {
                parts.push(current);
            }
            return parts;
        };
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if started {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                // Внутри одинарных кавычек ничего не экранируется (POSIX);
                // незакрытая кавычка поглощает остаток строки.
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                started = true;
                // В двойных кавычках экранируются только `"` и `\`
                // (семантика POSIX: слэш перед прочими символами —
                // literal, пути Windows сохраняются); незакрытая — до
                // конца строки.
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(esc @ ('"' | '\\')) => current.push(esc),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            // Хвостовой слэш в незакрытой кавычке.
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                started = true;
                // Вне кавычек слэш экранирует пробелы/кавычки/слэш,
                // перед прочими символами — literal (пути Windows).
                match chars.next() {
                    Some(esc) if escapable(esc) => current.push(esc),
                    Some(other) => {
                        current.push('\\');
                        current.push(other);
                    }
                    // Хвостовой слэш без пары — literal.
                    None => current.push('\\'),
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
}

/// Запускает команду `expose.launch` после перехода бутылки в `up`
/// (спека §7.1): argv из [`split_command_line`], вывод не ждём, ошибка
/// запуска — только журнал (launch не является частью контракта `up`).
pub(crate) fn spawn_launch(command: &str) {
    let mut argv = split_command_line(command).into_iter();
    let Some(program) = argv.next() else {
        tracing::warn!(command, "launch пуст — не запущен");
        return;
    };
    let mut process = std::process::Command::new(program);
    process.args(argv);
    // Демон может быть запущен из GUI без консоли: без этого флага
    // консольный launch-процесс показывал бы окно консоли.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match process.spawn() {
        Ok(child) => tracing::info!(command, pid = child.id(), "launch запущен"),
        Err(error) => tracing::warn!(command, %error, "не удалось запустить launch"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Векторная проверка: строка → ожидаемый argv.
    fn check(line: &str, expected: &[&str]) {
        assert_eq!(
            split_command_line(line),
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "строка {line:?}"
        );
    }

    #[test]
    fn plain_command_splits_on_whitespace() {
        check("mstsc /v:127.0.0.1:5", &["mstsc", "/v:127.0.0.1:5"]);
        check(
            "vpn connect office --silent",
            &["vpn", "connect", "office", "--silent"],
        );
        // Разные пробельные символы, повторные пробелы.
        check("  a \t b  ", &["a", "b"]);
    }

    #[test]
    fn empty_and_blank_lines_give_no_args() {
        check("", &[]);
        check("   ", &[]);
        check("\t \n", &[]);
    }

    #[test]
    fn double_quoted_argument_with_space_stays_single() {
        check(
            r#""C:\Program Files\Vendor\vpn.exe" connect"#,
            &[r"C:\Program Files\Vendor\vpn.exe", "connect"],
        );
        check(
            r#"copy "a b.txt" "c d.txt""#,
            &["copy", "a b.txt", "c d.txt"],
        );
        // Слэши путей Windows не требуют двойного экранирования:
        // перед «обычными» символами слэш literal (семантика POSIX).
        check(r#""a\b\c d""#, &[r"a\b\c d"]);
        check(
            r"C:\Program Files\unquoted",
            &[r"C:\Program", r"Files\unquoted"],
        );
    }

    #[test]
    fn single_quoted_argument_with_space_stays_single() {
        check(r"'/opt/my app/vpn' --flag", &["/opt/my app/vpn", "--flag"]);
        // Внутри одинарных кавычек ничего не экранируется (POSIX).
        check(r"printf 'a \ b'", &["printf", r"a \ b"]);
    }

    #[test]
    fn escaped_space_does_not_split() {
        check(r"a\ b c", &["a b", "c"]);
        check(r#""a\"b" c"#, &[r#"a"b"#, "c"]);
        check(r"a\\b", &[r"a\b"]);
        // Хвостовой слэш — literal, не теряет аргумент.
        check("dir\\", &["dir\\"]);
    }

    #[test]
    fn quotes_glue_fragments_and_empty_quotes_give_empty_arg() {
        check(r#"a"b c"d"#, &["ab cd"]);
        check("a''b", &["ab"]);
        check("cmd '' arg", &["cmd", "", "arg"]);
    }

    #[test]
    fn unclosed_quote_consumes_rest_of_line() {
        check(r#""abc def"#, &["abc def"]);
        check("'abc", &["abc"]);
    }
}
