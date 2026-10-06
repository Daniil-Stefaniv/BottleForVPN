// Тесты doctor-панели (M4 Task 4): таблица ✓/⚠/✗ из --json doctor,
// «Собрать логи» → logs export → путь показан.

import { beforeEach, describe, expect, it } from "vitest";
import { collectLogs, renderDoctorTable } from "./doctor";
import { setBottleInvoker } from "./ipc";

const DOCTOR_HTML = `
<table id="doctor-table"></table>
<button id="logs-export">Собрать логи</button>
<p id="logs-path" hidden></p>`;

describe("renderDoctorTable", () => {
  beforeEach(() => {
    document.body.innerHTML = DOCTOR_HTML;
  });

  it("таблица несёт глифы статусов и русский текст проверок", () => {
    renderDoctorTable([
      { code: "wsl", status: "Ok", detail: "WSL включён, дистрибуций: 2" },
      { code: "service", status: "Warn", detail: "Служба маршрутов не установлена" },
      { code: "agent", status: "Fail", detail: "агент не отвечает" },
    ]);
    const rows = document.querySelectorAll("#doctor-table tbody tr");
    expect(rows).toHaveLength(3);
    const text = document.querySelector("#doctor-table")?.textContent ?? "";
    expect(text).toContain("✓");
    expect(text).toContain("⚠");
    expect(text).toContain("✗");
    expect(text).toContain("WSL включён");
    expect(text).toContain("агент не отвечает");
  });

  it("пустой отчёт — строка-заглушка, не пустая таблица", () => {
    renderDoctorTable([]);
    const text = document.querySelector("#doctor-table")?.textContent ?? "";
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
