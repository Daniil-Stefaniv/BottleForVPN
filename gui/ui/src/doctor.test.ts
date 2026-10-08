// Тесты doctor-панели: чек-лист ✓/⚠/✗ из --json doctor со сводкой,
// «Собрать логи» → logs export → путь показан.

import { beforeEach, describe, expect, it } from "vitest";
import { collectLogs, renderDoctorChecks } from "./doctor";
import { setBottleInvoker } from "./ipc";

const DOCTOR_HTML = `
<p id="doctor-summary"></p>
<div id="doctor-list"></div>
<button id="logs-export">Собрать логи</button>
<p id="logs-path" hidden></p>`;

describe("renderDoctorChecks", () => {
  beforeEach(() => {
    document.body.innerHTML = DOCTOR_HTML;
  });

  it("чек-лист несёт глифы статусов, классы и русский текст проверок", () => {
    renderDoctorChecks([
      { code: "wsl", status: "Ok", detail: "WSL включён, дистрибуций: 2" },
      { code: "service", status: "Warn", detail: "Служба маршрутов не установлена" },
      { code: "agent", status: "Fail", detail: "агент не отвечает" },
    ]);
    const rows = document.querySelectorAll("#doctor-list .check-row");
    expect(rows).toHaveLength(3);
    expect(rows[0]?.className).toContain("status-ok");
    expect(rows[1]?.className).toContain("status-warn");
    expect(rows[2]?.className).toContain("status-fail");
    const text = document.querySelector("#doctor-list")?.textContent ?? "";
    expect(text).toContain("✓");
    expect(text).toContain("⚠");
    expect(text).toContain("✗");
    expect(text).toContain("WSL включён");
    expect(text).toContain("агент не отвечает");
  });

  it("сводка считает ок/предупреждения/ошибки", () => {
    renderDoctorChecks([
      { code: "a", status: "Ok", detail: "ок" },
      { code: "b", status: "Ok", detail: "ок" },
      { code: "c", status: "Warn", detail: "хм" },
      { code: "d", status: "Fail", detail: "плохо" },
    ]);
    expect(document.querySelector("#doctor-summary")?.textContent).toBe(
      "2 ок · 1 предупреждений · 1 ошибок",
    );
  });

  it("пустой отчёт — строка-заглушка, не пустой список", () => {
    renderDoctorChecks([]);
    const text = document.querySelector("#doctor-list")?.textContent ?? "";
    expect(text).toContain("проверок нет");
  });
});

describe("collectLogs", () => {
  beforeEach(() => {
    document.body.innerHTML = DOCTOR_HTML;
  });

  it("logs export → путь показан в #logs-path", async () => {
    const calls: string[][] = [];
    setBottleInvoker(async (args) => {
      calls.push(args);
      return {
        stdout: JSON.stringify({
          Path: "/tmp/bottleforapps-logs-20261005-1200.zip",
        }),
        code: 0,
      };
    });
    const path = await collectLogs();
    expect(calls).toEqual([["--json", "logs", "export"]]);
    expect(path).toBe("/tmp/bottleforapps-logs-20261005-1200.zip");
    const shown = document.querySelector<HTMLElement>("#logs-path");
    expect(shown?.hidden).toBe(false);
    expect(shown?.textContent).toContain("/tmp/bottleforapps-logs-20261005-1200.zip");
  });

  it("ошибка экспорта — путь скрыт, исключение прокинуто для тоста", async () => {
    setBottleInvoker(async () => ({
      stdout: JSON.stringify({ Error: { message: "нет журналов" } }),
      code: 1,
    }));
    await expect(collectLogs()).rejects.toThrowError("нет журналов");
    expect(document.querySelector<HTMLElement>("#logs-path")?.hidden).toBe(
      true,
    );
  });
});
