// Тесты сетки бутылок (0.3, макет v5): рендер без демона (мета
// «демон выкл.», Старт погашен), «Логин и пароль» погашена у WireGuard
// и host, спиннер действия, точечное обновление узлов между рендерами.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { renderBottleGrid, type BottleRow, type GridHandlers } from "./list";

const row = (over: Partial<BottleRow>): BottleRow => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  protocol: "wireguard",
  hasCredentials: false,
  state: "stopped",
  socks5: null,
  forwards: [],
  ...over,
});

function mount(): HTMLElement {
  document.body.innerHTML = `
    <div id="bottle-grid"></div>
    <p id="grid-empty" hidden></p>`;
  return document.querySelector<HTMLElement>("#bottle-grid")!;
}

function handlers(over: Partial<GridHandlers> = {}): GridHandlers {
  return {
    onAction: vi.fn(),
    onEdit: vi.fn(),
    onCreds: vi.fn(),
    ...over,
  };
}

const cell = (item: Element, role: string): HTMLButtonElement =>
  item.querySelector<HTMLButtonElement>(`[data-role=${role}]`)!;

beforeEach(() => {
  document.body.innerHTML = "";
});

describe("renderBottleGrid", () => {
  it("без демона: мета «демон выкл.», Старт погашен (.off + disabled)", () => {
    mount();
    renderBottleGrid([row({})], true, new Map(), handlers());

    const item = document.querySelector<HTMLElement>(".grid-row")!;
    expect(item.querySelector(".grid-meta")!.textContent).toBe("демон выкл.");
    const action = cell(item, "action");
    expect(action.classList.contains("off")).toBe(true);
    expect(action.disabled).toBe(true);
    expect(action.title).toBe("демон не запущен");
    // Пилюля без ложного состояния.
    expect(item.querySelector("[data-role=state]")!.textContent).toBe("—");
  });

  it("«Логин и пароль» погашена у WireGuard с пояснением", () => {
    mount();
    renderBottleGrid([row({ protocol: "wireguard" })], false, new Map(), handlers());
    const creds = cell(document.querySelector(".grid-row")!, "creds");
    expect(creds.classList.contains("off")).toBe(true);
    expect(creds.disabled).toBe(true);
    expect(creds.title).toContain("WireGuard");
  });

  it("«Логин и пароль» погашена у host-бутылки, работает у OpenVPN", () => {
    mount();
    const onCreds = vi.fn();
    renderBottleGrid(
      [
        row({ id: "h", kind: "host", protocol: null }),
        row({ id: "o", protocol: "openvpn" }),
      ],
      false,
      new Map(),
      handlers({ onCreds }),
    );
    const [host, openvpn] = Array.from(
      document.querySelectorAll<HTMLElement>(".grid-row"),
    );
    expect(cell(host, "creds").disabled).toBe(true);
    expect(cell(host, "creds").title).toContain("host");
    const openvpnCreds = cell(openvpn, "creds");
    expect(openvpnCreds.disabled).toBe(false);
    openvpnCreds.click();
    expect(onCreds).toHaveBeenCalledWith("o");
  });

  it("работающая бутылка: кнопка «Стоп» (halt), stopped — «Старт» (go)", () => {
    mount();
    renderBottleGrid(
      [row({ id: "up-b", state: "up", socks5: 1080 }), row({ id: "down-b" })],
      false,
      new Map(),
      handlers(),
    );
    const [up, down] = Array.from(
      document.querySelectorAll<HTMLElement>(".grid-row"),
    );
    const upAction = cell(up, "action");
    expect(upAction.textContent).toBe("Стоп");
    expect(upAction.classList.contains("halt")).toBe(true);
    const downAction = cell(down, "action");
    expect(downAction.textContent).toBe("Старт");
    expect(downAction.classList.contains("go")).toBe(true);
  });

  it("клик по Старт/Стоп вызывает onAction с правильным действием", () => {
    mount();
    const onAction = vi.fn();
    renderBottleGrid([row({ state: "up" })], false, new Map(), handlers({ onAction }));
    cell(document.querySelector(".grid-row")!, "action").click();
    expect(onAction).toHaveBeenCalledWith("office-a", "stop");
  });

  it("pending: кнопка со спиннером «ждите…», клики заблокированы", () => {
    mount();
    const pending = new Map([["office-a", "start" as const]]);
    renderBottleGrid([row({})], false, pending, handlers());
    const action = cell(document.querySelector(".grid-row")!, "action");
    expect(action.textContent).toBe("ждите…");
    expect(action.disabled).toBe(true);
  });

  it("переходные состояния блокируют кнопку", () => {
    mount();
    renderBottleGrid([row({ state: "starting" })], false, new Map(), handlers());
    expect(cell(document.querySelector(".grid-row")!, "action").disabled).toBe(true);
  });

  it("узлы строк живут между рендерами (точечное обновление)", () => {
    mount();
    renderBottleGrid([row({ id: "a" }), row({ id: "b" })], false, new Map(), handlers());
    const firstA = document.querySelector<HTMLElement>("[data-bottle-id=a]")!;
    renderBottleGrid(
      [row({ id: "b", state: "up" }), row({ id: "a", state: "up" })],
      false,
      new Map(),
      handlers(),
    );
    const afterA = document.querySelector<HTMLElement>("[data-bottle-id=a]")!;
    expect(afterA).toBe(firstA); // узел тот же — фокус/hover не сброшены
    // Порядок следует за rows.
    const order = Array.from(document.querySelectorAll(".grid-row")).map(
      (n) => (n as HTMLElement).dataset.bottleId,
    );
    expect(order).toEqual(["b", "a"]);
  });

  it("удалённые бутылки исчезают из сетки, пустая вкладка — заглушка", () => {
    mount();
    renderBottleGrid([row({})], false, new Map(), handlers());
    expect(document.querySelectorAll(".grid-row")).toHaveLength(1);
    renderBottleGrid([], false, new Map(), handlers());
    expect(document.querySelectorAll(".grid-row")).toHaveLength(0);
    expect(document.querySelector<HTMLElement>("#grid-empty")!.hidden).toBe(false);
  });

  it("причина failed уходит в title пилюли", () => {
    mount();
    renderBottleGrid(
      [row({ state: "failed", reason: "handshake timeout" })],
      false,
      new Map(),
      handlers(),
    );
    const pill = document.querySelector<HTMLElement>("[data-role=state]")!;
    expect(pill.textContent).toBe("failed");
    expect(pill.title).toBe("handshake timeout");
  });
});
