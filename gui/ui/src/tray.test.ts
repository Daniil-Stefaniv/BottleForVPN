// Тесты tray-модуля (M4 Task 5): подписи пунктов из строк таблицы,
// dispatch действий меню (старт/стоп/show/more).

import { describe, expect, it } from "vitest";
import { buildTrayEntries, handleTrayEvent } from "./tray";
import type { BottleRow } from "./list";

const row = (over: Partial<BottleRow>): BottleRow => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  state: "up",
  socks5: null,
  forwards: [],
  ...over,
});

describe("buildTrayEntries", () => {
  it("подпись: id — имя (состояние); running только для up", () => {
    const entries = buildTrayEntries([
      row({ id: "a", name: "Офис А", state: "up" }),
      row({ id: "b", name: "Корп", state: "stopped" }),
      row({ id: "c", name: "", state: "degraded" }),
    ]);
    expect(entries[0]).toEqual({
      id: "a",
      label: "a — Офис А (работает)",
      running: true,
    });
    expect(entries[1]?.running).toBe(false);
    expect(entries[1]?.label).toBe("b — Корп (остановлена)");
    expect(entries[2]?.label).toBe("c — (без имени) (деградация)");
  });

  it("пустой список — пустой набор записей", () => {
    expect(buildTrayEntries([])).toEqual([]);
  });
});

describe("handleTrayEvent", () => {
  it("старт/стоп маршрутизируются в действие бутылки", () => {
    const actions: { id: string; action: string }[] = [];
    handleTrayEvent({ kind: "start", id: "a" }, {
      onBottleAction: (id, action) => actions.push({ id, action }),
    });
    handleTrayEvent({ kind: "stop", id: "b" }, {
      onBottleAction: (id, action) => actions.push({ id, action }),
    });
    expect(actions).toEqual([
      { id: "a", action: "start" },
      { id: "b", action: "stop" },
    ]);
  });

  it("more/прочее не трогают бутылки", () => {
    const calls: unknown[] = [];
    handleTrayEvent({ kind: "more" }, {
      onBottleAction: () => calls.push("called"),
    });
    expect(calls).toEqual([]);
  });
});
