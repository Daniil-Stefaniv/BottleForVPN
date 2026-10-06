// Тесты экрана списка (M4 Task 3): merge list+status по id, отображаемые
// состояния/порты, блокировка кнопок в переходах, рендер таблицы.

import { beforeEach, describe, expect, it } from "vitest";
import {
  bottleActionsLocked,
  mergeListStatus,
  renderListTable,
  stateLabel,
} from "./list";
import type { BottleSummary, RuntimeState } from "./ipc";

const summary = (over: Partial<BottleSummary>): BottleSummary => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  state: null,
  ...over,
});

const runtime = (over: Partial<RuntimeState>): RuntimeState => ({
  id: "office-a",
  state: "Up",
  socks5: 1080,
  forwards: [],
  ...over,
});

describe("mergeListStatus", () => {
  it("объединяет list и status по id: рантайм важнее, socks5/forwards из него", () => {
    const rows = mergeListStatus(
      [summary({ id: "a", state: null }), summary({ id: "b" })],
      [
        runtime({ id: "a", state: "Starting", socks5: 55001 }),
        runtime({ id: "ghost", socks5: 999 }),
      ],
    );
    expect(rows).toEqual([
      {
        id: "a",
        name: "Офис А",
        kind: "env",
        state: "starting",
        reason: undefined,
        socks5: 55001,
        forwards: [],
      },
      {
        id: "b",
        name: "Офис А",
        kind: "env",
        state: "stopped",
        reason: undefined,
        socks5: null,
        forwards: [],
      },
      {
        id: "ghost",
        name: "",
        kind: "env",
        state: "up",
        reason: undefined,
        socks5: 999,
        forwards: [],
      },
    ]);
  });

  it("runtime без профиля тоже показывается (сирота из T13)", () => {
    const rows = mergeListStatus(
      [],
      [runtime({ id: "ghost", state: "Up" })],
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]?.id).toBe("ghost");
    expect(rows[0]?.name).toBe(""); // профиля нет — имя пустое
  });

  it("Degraded/Failed-объекты состояния нормализуются в канонические имена", () => {
    const rows = mergeListStatus(
      [summary({ id: "d" }), summary({ id: "f" })],
      [
        runtime({ id: "d", state: { Degraded: { reason: "агент молчит" } } }),
        runtime({ id: "f", state: { Failed: { reason: "туннель не поднялся" } } }),
      ],
    );
    expect(rows[0]?.state).toBe("degraded");
    expect(rows[1]?.state).toBe("failed");
  });

  it("пустые list и status — пустая таблица", () => {
    expect(mergeListStatus([], [])).toEqual([]);
  });
});

describe("stateLabel", () => {
  it("русские подписи состояний с причиной деградации/отказа", () => {
    expect(stateLabel("up")).toBe("работает");
    expect(stateLabel("stopped")).toBe("остановлена");
    expect(stateLabel("starting")).toBe("поднимается");
    expect(stateLabel("stopping")).toBe("останавливается");
    expect(stateLabel("degraded", "агент молчит")).toBe(
      "деградация: агент молчит",
    );
    expect(stateLabel("failed", "туннель не поднялся")).toBe(
      "ошибка: туннель не поднялся",
    );
  });
});

describe("bottleActionsLocked", () => {
  it("переходы (starting/stopping) блокируют кнопки", () => {
    expect(bottleActionsLocked("starting")).toBe(true);
    expect(bottleActionsLocked("stopping")).toBe(true);
  });

  it("устойчивые состояния не блокируют", () => {
    for (const state of ["up", "stopped", "degraded", "failed"]) {
      expect(bottleActionsLocked(state)).toBe(false);
    }
  });
});

describe("renderListTable", () => {
  beforeEach(() => {
    document.body.innerHTML = '<table id="bottles"></table>';
  });

  it("рисует строки: колонки id/name/kind/state/socks5/forwards", () => {
    renderListTable(
      [
        {
          id: "office-a",
          name: "Офис А",
          kind: "env",
          state: "up",
          socks5: 1080,
          forwards: [{ port: 33389, target: "10.20.30.15:3389" }],
        },
      ],
      () => {},
    );
    const rows = document.querySelectorAll<HTMLTableRowElement>("#bottles tbody tr");
    expect(rows).toHaveLength(1);
    const cells = rows[0]?.querySelectorAll("td") ?? [];
    expect(cells[0]?.textContent).toBe("office-a");
    expect(cells[1]?.textContent).toBe("Офис А");
    expect(cells[2]?.textContent).toBe("env");
    expect(cells[3]?.textContent).toBe("работает");
    expect(cells[4]?.textContent).toBe("1080");
    expect(cells[5]?.textContent).toContain("33389→10.20.30.15:3389");
  });

  it("переходное состояние блокирует кнопки (disabled)", () => {
    renderListTable(
      [
        {
          id: "a",
          name: "A",
          kind: "env",
          state: "starting",
          socks5: null,
          forwards: [],
        },
      ],
      () => {},
    );
    const buttons = document.querySelectorAll<HTMLButtonElement>(
      "#bottles tbody button",
    );
    for (const button of buttons) {
      expect(button.disabled).toBe(true);
    }
  });

  it("кнопки старт/стоп вызывают действие с id бутылки", () => {
    const calls: { id: string; action: string }[] = [];
    renderListTable(
      [
        {
          id: "a",
          name: "A",
          kind: "env",
          state: "stopped",
          socks5: null,
          forwards: [],
        },
      ],
      (id, action) => calls.push({ id, action }),
    );
    const [start] = [
      ...document.querySelectorAll<HTMLButtonElement>("#bottles tbody button"),
    ];
    start?.click();
    expect(calls).toEqual([{ id: "a", action: "start" }]);
  });

  it("кнопка «Изменить» открывает форму edit с id бутылки", () => {
    const edits: string[] = [];
    renderListTable(
      [
        {
          id: "a",
          name: "A",
          kind: "env",
          state: "stopped",
          socks5: null,
          forwards: [],
        },
      ],
      () => {},
      (id) => edits.push(id),
    );
    const buttons = [
      ...document.querySelectorAll<HTMLButtonElement>("#bottles tbody button"),
    ];
    const edit = buttons.find((button) => button.textContent === "Изменить");
    edit?.click();
    expect(edits).toEqual(["a"]);
  });
});
