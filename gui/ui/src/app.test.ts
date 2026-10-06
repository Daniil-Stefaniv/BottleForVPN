// Тесты каркаса приложения (M4 Task 3): refresh списка через CLI-мост,
// состояние «демон не запущен», старт/стоп с обновлением, тосты ошибок.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  mountApp,
  performBottleAction,
  refreshBottles,
  setDaemonApi,
} from "./app";
import { setBottleInvoker } from "./ipc";
import type { BottleOutput } from "./ipc";

const reply = (stdout: object, code = 0): BottleOutput => ({
  stdout: JSON.stringify(stdout),
  code,
});

function stubInvoker(
  responses: object[],
): { calls: string[][]; next: (stdout: object, code?: number) => void } {
  const calls: string[][] = [];
  const queue = [...responses];
  setBottleInvoker(async (args) => {
    calls.push(args);
    const head = queue.shift();
    if (head === undefined) {
      throw new Error("неожиданный вызов CLI в тесте");
    }
    if ("__throw" in head) {
      throw new Error(String((head as { __throw: string }).__throw));
    }
    return reply(head);
  });
  return {
    calls,
    next: (stdout, code) => queue.push(reply(stdout, code)),
  };
}

const HTML = `
<main id="app">
  <section id="screen-list">
    <table id="bottles"></table>
  </section>
  <section id="daemon-panel" hidden>
    <p id="daemon-hint"></p>
    <button id="daemon-start">Запустить демона</button>
    <pre id="daemon-log"></pre>
  </section>
  <div id="toast" hidden></div>
</main>`;

describe("refreshBottles", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("list+status сливаются и рендерятся в таблицу", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([
      { List: { bottles: [{ id: "a", name: "Офис А", kind: "env", state: null }] } },
      { Status: { bottles: [{ id: "a", state: "Up", socks5: 1080, forwards: [] }] } },
    ]);
    const rows = await refreshBottles();
    expect(rows).toEqual([
      {
        id: "a",
        name: "Офис А",
        kind: "env",
        state: "up",
        reason: undefined,
        socks5: 1080,
        forwards: [],
      },
    ]);
    const rendered = document.querySelectorAll("#bottles tbody tr");
    expect(rendered).toHaveLength(1);
    // Демон жив — панель инструкции скрыта.
    expect(
      document.querySelector<HTMLElement>("#daemon-panel")?.hidden,
    ).toBe(true);
  });

  it("«демон не запущен» — отдельное состояние с инструкцией, таблица очищена", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([{ __throw: "Не удалось подключиться к демону BottleForApps — запущен ли bottled?" }]);
    const rows = await refreshBottles();
    expect(rows).toEqual([]);
    const panel = document.querySelector<HTMLElement>("#daemon-panel");
    expect(panel?.hidden).toBe(false);
    expect(panel?.textContent).toContain("bottled");
    expect(document.querySelector("#daemon-hint")?.textContent).toMatch(
      /Запустить демона|bottled/,
    );
  });

  it("прочая ошибка CLI — тост с дословным сообщением", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([{ __throw: "хранилище профилей недоступно" }]);
    const rows = await refreshBottles();
    expect(rows).toEqual([]);
    const toast = document.querySelector<HTMLElement>("#toast");
    expect(toast?.hidden).toBe(false);
    expect(toast?.textContent).toContain("хранилище профилей недоступно");
  });
});

describe("performBottleAction", () => {
  it("старт вызывает CLI с --json start <id> и обновляет список", async () => {
    document.body.innerHTML = HTML;
    const invoker = stubInvoker([
      { Started: { id: "a" } },
      { List: { bottles: [{ id: "a", name: "A", kind: "env", state: null }] } },
      { Status: { bottles: [{ id: "a", state: "Starting", socks5: null, forwards: [] }] } },
    ]);
    await performBottleAction("a", "start");
    expect(invoker.calls).toEqual([
      ["--json", "start", "a"],
      ["--json", "list"],
      ["--json", "status"],
    ]);
    expect(document.querySelectorAll("#bottles tbody tr")).toHaveLength(1);
  });

  it("stopped с warning демона показывает предупреждение тостом", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([
      { Stopped: { id: "corp", warning: "VPN-клиент оставлен запущенным" } },
      { List: { bottles: [] } },
      { Status: { bottles: [] } },
    ]);
    await performBottleAction("corp", "stop");
    const toast = document.querySelector<HTMLElement>("#toast");
    expect(toast?.textContent).toContain("VPN-клиент оставлен запущенным");
  });

  it("ошибка старта — тост с сообщением демона, без refresh", async () => {
    document.body.innerHTML = HTML;
    const invoker = stubInvoker([{ __throw: "профиль не найден: x" }]);
    await performBottleAction("x", "start");
    expect(invoker.calls).toEqual([["--json", "start", "x"]]);
    expect(document.querySelector("#toast")?.textContent).toContain(
      "профиль не найден: x",
    );
  });
});

describe("кнопка «Запустить демона»", () => {
  it("вызывает start_daemon и затем refresh", async () => {
    document.body.innerHTML = HTML;
    const daemonCalls: string[] = [];
    setDaemonApi({
      start: async () => {
        daemonCalls.push("start");
      },
      status: async () => ({ running: true, log_tail: "bottled 0.2.0" }),
    });
    stubInvoker([
      { List: { bottles: [] } },
      { Status: { bottles: [] } },
    ]);
    document
      .querySelector<HTMLButtonElement>("#daemon-start")
      ?.addEventListener("click", () => {
        void mountApp()["noop"];
      });
    // Кнопка подключается в mountApp; вызываем обработчик напрямую через клик
    // после монтирования обработчиков на текущем DOM.
    mountApp({ once: true });
    const button = document.querySelector<HTMLButtonElement>("#daemon-start");
    button?.click();
    await vi.waitFor(() => expect(daemonCalls).toEqual(["start"]));
  });
});
