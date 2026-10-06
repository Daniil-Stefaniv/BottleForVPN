// Doctor-панель (M4 Task 4): `bottle --json doctor` → таблица ✓/⚠/✗,
// кнопка «Собрать логи» → `logs export` → путь + «Открыть папку»
// (Tauri opener — адаптер подключается из main.ts).

import { runBottleReply } from "./ipc";
import type { CheckItem } from "./ipc";

/** Глифы статусов — как в текстовом render_doctor CLI. */
const STATUS_GLYPH: Record<CheckItem["status"], string> = {
  Ok: "✓",
  Warn: "⚠",
  Fail: "✗",
};

/** Рисует отчёт doctor в `#doctor-table`: глиф, код проверки, пояснение. */
export function renderDoctorTable(items: CheckItem[]): void {
  const table = document.querySelector<HTMLTableElement>("#doctor-table");
  if (table === null) {
    return;
  }
  table.innerHTML = "";
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const title of ["", "Проверка", "Результат"]) {
    const th = document.createElement("th");
    th.textContent = title;
    headRow.append(th);
  }
  thead.append(headRow);
  const tbody = document.createElement("tbody");
  if (items.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 3;
    td.textContent = "проверок нет";
    tr.append(td);
    tbody.append(tr);
  }
  for (const item of items) {
    const tr = document.createElement("tr");
    const glyph = document.createElement("td");
    glyph.textContent = STATUS_GLYPH[item.status];
    glyph.className = `status-${item.status.toLowerCase()}`;
    const code = document.createElement("td");
    code.textContent = item.code;
    const detail = document.createElement("td");
    detail.textContent = item.detail;
    tr.append(glyph, code, detail);
    tbody.append(tr);
  }
  table.append(thead, tbody);
}

/** Загружает и рисует отчёт doctor; ошибки прокидываются для тоста. */
export async function loadDoctor(): Promise<void> {
  const reply = await runBottleReply(["--json", "doctor"]);
  if (!("DoctorReport" in reply)) {
    throw new Error("Неожиданный ответ демона на doctor");
  }
  renderDoctorTable(reply.DoctorReport.items);
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
