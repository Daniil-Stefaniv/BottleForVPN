// Тесты каркаса приложения: refresh списка через CLI-мост, master-detail
// рендер, состояние «демон не запущен», старт/стоп с busy-спиннером и
// обновлением, тосты ошибок/успеха, статус-бар.

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
  <section id="screen-list" class="screen">
    <span id="list-count"></span>
    <div id="bottle-list"></div>
    <div id="bottle-detail"></div>
  </section>
  <section id="daemon-panel" hidden>
    <p id="daemon-hint"></p>
    <button id="daemon-start">Запустить демона</button>
    <pre id="daemon-log"></pre>
  </section>
  <footer id="statusbar">
    <span id="statusbar-daemon"></span>
    <span id="statusbar-active"></span>
    <span id="statusbar-updated"></span>
  </footer>
  <div id="toast" hidden></div>
</main>`;

const listReply = {
  List: { bottles: [{ id: "a", name: "Офис А", kind: "env", state: null }] },
};
const statusReply = {
  Status: { bottles: [{ id: "a", state: "Up", socks5: 1080, forwards: [] }] },
};

describe("refreshBottles", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("list+status сливаются и рендерятся в master-список и detail", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([listReply, statusReply]);
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
    expect(document.querySelectorAll(".bottle-item")).toHaveLength(1);
    // Первая (и единственная) строка авто-выбрана — detail показывает её.
    expect(document.querySelector(".bottle-item")?.classList.contains("selected")).toBe(true);
    expect(document.querySelector("#bottle-detail")?.textContent).toContain("Офис А");
    // Демон жив — панель инструкции скрыта.
    expect(
      document.querySelector<HTMLElement>("#daemon-panel")?.hidden,
    ).toBe(true);
  });

  it("повторный refresh не пересоздаёт узлы списка (точечный рендер)", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([listReply, statusReply, listReply, statusReply]);
    await refreshBottles();
    const before = document.querySelector<HTMLElement>(".bottle-item");
    await refreshBottles();
    const after = document.querySelector<HTMLElement>(".bottle-item");
    expect(after).toBe(before);
  });

  it("«демон не запущен» — отдельное состояние с инструкцией, список очищен", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([{ __throw: "Не удалось подключиться к демону BottleForApps — запущен ли bottled?" }]);
    const rows = await refreshBottles();
    expect(rows).toEqual([]);
    const panel = document.querySelector<HTMLElement>("#daemon-panel");
    expect(panel?.hidden).toBe(false);
    expect(document.querySelector("#daemon-hint")?.textContent).toMatch(
      /Запустить демона|bottled/,
    );
    expect(document.querySelectorAll(".bottle-item")).toHaveLength(0);
    expect(document.querySelector("#statusbar-daemon")?.textContent).toContain(
      "демон остановлен",
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

  it("статус-бар: активные бутылки и демон работает", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([listReply, statusReply]);
    await refreshBottles();
    expect(document.querySelector("#statusbar-active")?.textContent).toBe(
      "активных: 1",
    );
    expect(document.querySelector("#statusbar-daemon")?.textContent).toContain(
      "демон работает",
    );
    expect(
      document.querySelector("#statusbar-updated")?.textContent,
    ).toContain("обновлено");
  });
});

describe("performBottleAction", () => {
  it("старт вызывает CLI с --json start <id> и обновляет список", async () => {
    document.body.innerHTML = HTML;
    const invoker = stubInvoker([
      { Started: { id: "a" } },
      listReply,
      statusReply,
    ]);
    await performBottleAction("a", "start");
    expect(invoker.calls).toEqual([
      ["--json", "start", "a"],
      ["--json", "list"],
      ["--json", "status"],
    ]);
    expect(document.querySelectorAll(".bottle-item")).toHaveLength(1);
  });

  it("на время CLI-вызова кнопка показывает спиннер (pending)", async () => {
    document.body.innerHTML = HTML;
    let release: (output: BottleOutput) => void = () => {};
    const calls: string[][] = [];
    setBottleInvoker((args) => {
      calls.push(args);
      if (args[1] === "start") {
        return new Promise<BottleOutput>((resolve) => {
          release = resolve;
        });
      }
      return Promise.resolve(reply(args[1] === "list" ? listReply : statusReply));
    });
    await refreshBottles();
    const pending = performBottleAction("a", "start");
    // CLI-вызов в полёте — кнопка detail со спиннером и заблокирована.
    const button = document.querySelector<HTMLButtonElement>("#detail-action");
    expect(button?.disabled).toBe(true);
    expect(button?.textContent).toContain("ждите");
    release(reply({ Started: { id: "a" } }));
    await pending;
    expect(
      document.querySelector<HTMLButtonElement>("#detail-action")?.textContent,
    ).not.toContain("ждите");
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
    mountApp({ once: true });
    const button = document.querySelector<HTMLButtonElement>("#daemon-start");
    button?.click();
    await vi.waitFor(() => expect(daemonCalls).toEqual(["start"]));
  });
});

describe("nextPollDelayMs (I4: backoff при выключенном демоне)", () => {
  it("демон недоступен — медленный зонд 10 с; жив — 2 с", async () => {
    const { nextPollDelayMs } = await import("./app");
    expect(nextPollDelayMs(true)).toBe(10000);
    expect(nextPollDelayMs(false)).toBe(2000);
  });
});

describe("кнопки аплаенса и остановки демона", () => {
  // Фикстура с шапкой и doctor-экраном, где живут новые кнопки.
  const HTML_BUTTONS = `
  <main id="app">
    <header id="app-header">
      <nav id="tabs"></nav>
      <button id="daemon-stop" type="button">Остановить всё</button>
    </header>
    <section id="screen-list" class="screen">
      <span id="list-count"></span>
      <div id="bottle-list"></div>
      <div id="bottle-detail"></div>
    </section>
    <section id="screen-doctor" class="screen" hidden>
      <p id="doctor-summary" class="muted"></p>
      <div id="doctor-list"></div>
      <button id="appliance-upgrade" type="button">Обновить аплаенс</button>
      <button id="appliance-reset" type="button">Сбросить аплаенс</button>
      <p id="logs-path" hidden></p>
    </section>
    <section id="daemon-panel" hidden>
      <p id="daemon-hint"></p>
      <button id="daemon-start">Запустить демона</button>
      <pre id="daemon-log"></pre>
    </section>
    <footer id="statusbar">
      <span id="statusbar-daemon"></span>
      <span id="statusbar-active"></span>
      <span id="statusbar-updated"></span>
    </footer>
    <div id="toast" hidden></div>
  </main>`;

  const doctorReply = {
    DoctorReport: { items: [{ code: "wsl", status: "Ok", detail: "ок" }] },
  };

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("«Обновить аплаенс» шлёт upgrade и показывает версии тостом", async () => {
    document.body.innerHTML = HTML_BUTTONS;
    const invoker = stubInvoker([
      listReply,
      statusReply, // начальный refresh при mountApp
      { Upgraded: { previous: "0.2.4", current: "0.2.7" } },
      doctorReply,
      listReply,
      statusReply,
    ]);
    mountApp({ once: true });
    // Дожидаемся начального refresh (list+status), чтобы клик не
    // перехватил их ответы из очереди.
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "status"]),
    );
    document.querySelector<HTMLButtonElement>("#appliance-upgrade")?.click();
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "upgrade"]),
    );
    await vi.waitFor(() =>
      expect(document.querySelector("#toast")?.textContent).toContain(
        "0.2.4 → 0.2.7",
      ),
    );
  });

  it("«Сбросить аплаенс» без подтверждения ничего не шлёт, с подтверждением — upgrade --force", async () => {
    document.body.innerHTML = HTML_BUTTONS;
    const { setConfirmAction } = await import("./app");
    const invoker = stubInvoker([listReply, statusReply]);
    mountApp({ once: true });
    // Дожидаемся начального refresh, чтобы клик не перехватил его ответы.
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "status"]),
    );
    setConfirmAction(() => false);
    document.querySelector<HTMLButtonElement>("#appliance-reset")?.click();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(invoker.calls).not.toContainEqual(["--json", "upgrade", "--force"]);

    setConfirmAction(() => true);
    invoker.next({ Upgraded: { previous: null, current: "0.2.7" } });
    invoker.next(doctorReply);
    invoker.next(listReply);
    invoker.next(statusReply);
    document.querySelector<HTMLButtonElement>("#appliance-reset")?.click();
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "upgrade", "--force"]),
    );
  });

  it("«Остановить всё» с подтверждением шлёт daemon stop", async () => {
    document.body.innerHTML = HTML_BUTTONS;
    const { setConfirmAction } = await import("./app");
    setConfirmAction(() => true);
    const invoker = stubInvoker([
      listReply,
      statusReply, // начальный refresh при mountApp
      { Done: { warning: null } },
    ]);
    mountApp({ once: true });
    // Дожидаемся начального refresh, чтобы клик не перехватил его ответы.
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "status"]),
    );
    document.querySelector<HTMLButtonElement>("#daemon-stop")?.click();
    await vi.waitFor(() =>
      expect(invoker.calls).toContainEqual(["--json", "daemon", "stop"]),
    );
    await vi.waitFor(() =>
      expect(document.querySelector("#toast")?.textContent).toContain(
        "Демон останавливается",
      ),
    );
  });
});

describe("refreshBottles: частичные данные (M7)", () => {
  it("отказ status не прячет список — строки без рантайма", async () => {
    document.body.innerHTML = HTML;
    stubInvoker([
      listReply,
      { __throw: "менеджер бутылок не подключён" },
    ]);
    const rows = await refreshBottles();
    expect(rows).toHaveLength(1);
    expect(rows[0]?.state).toBe("stopped");
    expect(document.querySelectorAll(".bottle-item")).toHaveLength(1);
  });
});
