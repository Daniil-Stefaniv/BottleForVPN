// Doctor-панель: `bottle --json doctor` → чек-лист ✓/⚠/✗ со сводкой
// (ок/предупреждения/ошибки), кнопка «Собрать логи» → `logs export` →
// путь + «Открыть папку» (Tauri opener — адаптер подключается из main.ts).

import { runBottleReply } from "./ipc";
import type { CheckItem } from "./ipc";

/** Глифы статусов — как в текстовом render_doctor CLI. */
const STATUS_GLYPH: Record<CheckItem["status"], string> = {
  Ok: "✓",
  Warn: "⚠",
  Fail: "✗",
};

/**
 * Рисует отчёт doctor чек-листом в `#doctor-list` (строки .check-row со
 * статус-классом) и сводку «N ок · N предупреждений · N ошибок» в
 * `#doctor-summary`.
 */
export function renderDoctorChecks(items: CheckItem[]): void {
  const list = document.querySelector<HTMLElement>("#doctor-list");
  if (list === null) {
    return;
  }
  list.innerHTML = "";
  if (items.length === 0) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "проверок нет";
    list.append(empty);
  }
  for (const item of items) {
    const row = document.createElement("div");
    row.className = `check-row status-${item.status.toLowerCase()}`;
    const glyph = document.createElement("span");
    glyph.className = "check-glyph";
    glyph.textContent = STATUS_GLYPH[item.status];
    const body = document.createElement("div");
    const code = document.createElement("div");
    code.className = "check-code";
    code.textContent = item.code;
    const detail = document.createElement("div");
    detail.className = "check-detail";
    detail.textContent = item.detail;
    body.append(code, detail);
    row.append(glyph, body);
    list.append(row);
  }
  const summary = document.querySelector<HTMLElement>("#doctor-summary");
  if (summary !== null) {
    const ok = items.filter((item) => item.status === "Ok").length;
    const warn = items.filter((item) => item.status === "Warn").length;
    const fail = items.filter((item) => item.status === "Fail").length;
    summary.textContent = `${ok} ок · ${warn} предупреждений · ${fail} ошибок`;
  }
}

/** Загружает и рисует отчёт doctor; ошибки прокидываются для тоста. */
export async function loadDoctor(): Promise<void> {
  const reply = await runBottleReply(["--json", "doctor"]);
  if (!("DoctorReport" in reply)) {
    throw new Error("Неожиданный ответ демона на doctor");
  }
  renderDoctorChecks(reply.DoctorReport.items);
}

/**
 * «Собрать логи»: `logs export` → Path; путь показывается в
 * `#logs-path`. Возвращает путь (для кнопки «Открыть папку»).
 */
export async function collectLogs(): Promise<string> {
  const reply = await runBottleReply(["--json", "logs", "export"]);
  if (!("Path" in reply)) {
    throw new Error("Неожиданный ответ демона на logs export");
  }
  const shown = document.querySelector<HTMLElement>("#logs-path");
  if (shown !== null) {
    shown.hidden = false;
    shown.textContent = `Журналы выгружены: ${reply.Path}`;
  }
  return reply.Path;
}
