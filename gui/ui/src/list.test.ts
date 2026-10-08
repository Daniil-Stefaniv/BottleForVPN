// Тесты экрана списка: merge list+status по id, подписи состояний,
// блокировка в переходах; точечный рендер master-списка (узлы строк
// живут между поллингами) и detail-панели (перерисовка по сигнатуре).

import { beforeEach, describe, expect, it } from "vitest";
import {
  bottleActionsLocked,
  mergeListStatus,
  renderBottleDetail,
  renderBottleList,
  resetDetailCache,
  stateDotClass,
  stateLabel,
  type BottleAction,
  type BottleRow,
  type DetailHandlers,
  type ListHandlers,
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

const row = (over: Partial<BottleRow>): BottleRow => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  state: "up",
  socks5: 1080,
  forwards: [],
  ...over,
});

const noopListHandlers = (over: Partial<ListHandlers> = {}): ListHandlers => ({
  onSelect: () => {},
  onCreate: () => {},
  ...over,
});

const noopDetailHandlers = (over: Partial<DetailHandlers> = {}): DetailHandlers => ({
  onAction: () => {},
  onEdit: () => {},
  onDelete: () => {},
  onNotify: () => {},
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

  it("пустые list и status — пустой список", () => {
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

describe("stateDotClass", () => {
  it("класс точки по состоянию; неизвестное — как stopped", () => {
    expect(stateDotClass("up")).toBe("dot dot-up");
    expect(stateDotClass("starting")).toBe("dot dot-starting");
    expect(stateDotClass("неведомое")).toBe("dot dot-stopped");
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

describe("renderBottleList (master)", () => {
  beforeEach(() => {
    document.body.innerHTML =
      '<span id="list-count"></span><div id="bottle-list"></div>';
  });

  it("рисует строки: имя, тип, SOCKS5/состояние; счётчик в шапке", () => {
    renderBottleList(
      [
        row({ socks5: 55123 }),
        row({ id: "b", name: "", state: "stopped", socks5: null }),
      ],
      null,
      noopListHandlers(),
    );
    const items = document.querySelectorAll<HTMLElement>(".bottle-item");
    expect(items).toHaveLength(2);
    expect(items[0]?.textContent).toContain("Офис А");
    expect(items[0]?.textContent).toContain("env");
    expect(items[0]?.textContent).toContain("SOCKS5 :55123 · работает");
    // Без имени — показывается id.
    expect(items[1]?.textContent).toContain("b");
    expect(items[1]?.textContent).toContain("остановлена");
    expect(document.querySelector("#list-count")?.textContent).toBe("Бутылки · 2");
  });

  it("точечное обновление: узлы строк живут между поллингами", () => {
    const handlers = noopListHandlers();
    renderBottleList([row({})], null, handlers);
    const before = document.querySelector<HTMLElement>(".bottle-item");
    renderBottleList([row({ state: "stopped", socks5: null })], null, handlers);
    const after = document.querySelector<HTMLElement>(".bottle-item");
    // Тот же DOM-узел — не пересоздан (фокус/hover не сбрасываются).
    expect(after).toBe(before);
    expect(after?.textContent).toContain("остановлена");
    expect(after?.querySelector(".dot")?.className).toBe("dot dot-stopped");
  });

  it("исчезнувшие бутылки удаляются, порядок совпадает со входом", () => {
    const handlers = noopListHandlers();
    renderBottleList([row({ id: "a" }), row({ id: "b" }), row({ id: "c" })], null, handlers);
    renderBottleList([row({ id: "c" }), row({ id: "a" })], null, handlers);
    const ids = [...document.querySelectorAll<HTMLElement>(".bottle-item")].map(
      (item) => item.dataset.bottleId,
    );
    expect(ids).toEqual(["c", "a"]);
  });

  it("выбранная строка помечается классом selected, клик зовёт onSelect", () => {
    const selected: string[] = [];
    renderBottleList(
      [row({ id: "a" }), row({ id: "b" })],
      "b",
      noopListHandlers({ onSelect: (id) => selected.push(id) }),
    );
    const items = document.querySelectorAll<HTMLElement>(".bottle-item");
    expect(items[0]?.classList.contains("selected")).toBe(false);
    expect(items[1]?.classList.contains("selected")).toBe(true);
    items[0]?.click();
    expect(selected).toEqual(["a"]);
  });
});

describe("renderBottleDetail (detail)", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="bottle-detail"></div>';
    resetDetailCache();
  });

  it("null — заглушка «выберите бутылку»", () => {
    renderBottleDetail(null, null, noopDetailHandlers());
    expect(document.querySelector("#detail-empty")?.textContent).toContain(
      "Выберите бутылку",
    );
  });

  it("параметры, пробросы и бейджи состояния", () => {
    renderBottleDetail(
      row({
        forwards: [{ port: 13389, target: "10.20.30.15:3389" }],
      }),
      null,
      noopDetailHandlers(),
    );
    const text = document.querySelector("#bottle-detail")?.textContent ?? "";
    expect(text).toContain("Офис А");
    expect(text).toContain("работает");
    expect(text).toContain("127.0.0.1:1080");
    expect(text).toContain("127.0.0.1:13389 → 10.20.30.15:3389");
  });

  it("повторный рендер без изменений не трогает DOM (сигнатура)", () => {
    const handlers = noopDetailHandlers();
    const data = row({});
    renderBottleDetail(data, null, handlers);
    const before = document.querySelector("#bottle-detail h2");
    renderBottleDetail(data, null, handlers);
    const after = document.querySelector("#bottle-detail h2");
    expect(after).toBe(before);
  });

  it("кнопка действия: у работающей — «Стоп», у остановленной — «Старт»", () => {
    const actions: BottleAction[] = [];
    const handlers = noopDetailHandlers({
      onAction: (_id, action) => actions.push(action),
    });
    renderBottleDetail(row({ state: "up" }), null, handlers);
    document.querySelector<HTMLButtonElement>("#detail-action")?.click();
    expect(actions).toEqual(["stop"]);

    resetDetailCache();
    renderBottleDetail(row({ state: "stopped" }), null, handlers);
    document.querySelector<HTMLButtonElement>("#detail-action")?.click();
    expect(actions).toEqual(["stop", "start"]);
  });

  it("pending: спиннер и заблокированная кнопка на время CLI-вызова", () => {
    renderBottleDetail(row({ state: "stopped" }), "start", noopDetailHandlers());
    const button = document.querySelector<HTMLButtonElement>("#detail-action");
    expect(button?.disabled).toBe(true);
    expect(button?.textContent).toContain("ждите");
  });

  it("переходное состояние блокирует кнопку действия", () => {
    renderBottleDetail(row({ state: "starting" }), null, noopDetailHandlers());
    expect(
      document.querySelector<HTMLButtonElement>("#detail-action")?.disabled,
    ).toBe(true);
  });

  it("удаление — двухшаговое: подтверждение → onDelete", () => {
    const deleted: string[] = [];
    const handlers = noopDetailHandlers({
      onDelete: (id) => deleted.push(id),
    });
    const data = row({});
    renderBottleDetail(data, null, handlers);
    const button = document.querySelector<HTMLButtonElement>("#detail-delete");
    expect(button?.textContent).toBe("Удалить");
    button?.click();
    const confirm = document.querySelector<HTMLButtonElement>("#detail-delete");
    expect(confirm?.textContent).toBe("Подтвердить удаление");
    expect(deleted).toEqual([]);
    confirm?.click();
    expect(deleted).toEqual(["office-a"]);
  });

  it("«Изменить» зовёт onEdit с id бутылки", () => {
    const edits: string[] = [];
    renderBottleDetail(
      row({}),
      null,
      noopDetailHandlers({ onEdit: (id) => edits.push(id) }),
    );
    document.querySelector<HTMLButtonElement>("#detail-edit")?.click();
    expect(edits).toEqual(["office-a"]);
  });
});
