// Тесты каркаса приложения 0.3: список из БД работает без демона,
// вкладки Env/Host фильтруют сетку, статусы — push-события + watchdog,
// командная панель (запуск/остановка демона, консоль), диалог
// «Логин и пароль». Мост GuiApi подменяется заглушкой (setGuiApi);
// модули перезагружаются в каждом тесте — состояние каркаса чистое.

import { afterEach, describe, expect, it, vi } from "vitest";
import type { GuiApi } from "./backend";
import type { BottleListRowWire, DaemonStatsWire } from "./ipc";

const ROWS: BottleListRowWire[] = [
  {
    id: "office-a",
    name: "Офис А",
    kind: "env",
    protocol: "wireguard",
    has_credentials: false,
  },
  {
    id: "office-b",
    name: "Офис Б",
    kind: "env",
    protocol: "openvpn",
    has_credentials: true,
  },
  {
    id: "legacy-ac",
    name: "Legacy AnyConnect",
    kind: "host",
    protocol: null,
    has_credentials: false,
  },
];

const STATS: DaemonStatsWire = {
  pid: 4242,
  version: "0.3.0-test",
  started_at: "2026-10-22T10:00:00Z",
  active_bottles: 0,
  total_profiles: ROWS.length,
  mem_bytes: 1024,
  cpu_pct: 0,
  wsl_state: "running",
  appliance_version: "0.3.0",
};

const DAEMON_DOWN = "не удалось подключиться к демону — запущен ли pogrebok-daemon?";

const HTML = `
<main id="app">
  <header id="app-header">
    <nav id="tabs">
      <button class="tab active" data-screen="env">Env · 0</button>
      <button class="tab" data-screen="host">Host · 0</button>
      <button class="tab" data-screen="doctor">Диагностика</button>
    </nav>
    <button id="create-open" type="button">+ Создать бутылку</button>
    <button id="settings-open" type="button">Настройки</button>
  </header>
  <section id="screen-bottles" class="screen screen-grid">
    <div id="bottle-grid"></div>
    <p id="grid-empty" hidden></p>
  </section>
  <section id="screen-doctor" class="screen" hidden>
    <p id="doctor-summary"></p>
    <button id="doctor-refresh" type="button">Проверить снова</button>
    <button id="appliance-upgrade" type="button">Обновить аплаенс</button>
    <button id="appliance-reset" type="button">Сбросить аплаенс</button>
    <div id="stats-cards"></div>
    <div id="console">
      <div class="chips" id="chips-src"></div>
      <div class="chips" id="chips-level"></div>
      <div class="chips" id="chips-cat"></div>
      <input id="console-search" />
      <button id="console-pause" type="button">Пауза</button>
      <div id="console-list"></div>
      <button id="logs-export" type="button">Собрать логи (ZIP)</button>
      <button id="logs-open-dir" type="button">Открыть папку логов</button>
      <span id="console-count"></span>
    </div>
    <div id="doctor-list"></div>
    <p id="logs-path" hidden></p>
  </section>
  <footer id="cmdbar">
    <span id="cmd-status"></span>
    <button id="cmd-daemon-start" type="button" hidden>Запустить демона</button>
    <button id="cmd-daemon-stop" type="button" hidden>Остановить демона</button>
    <button id="cmd-console" type="button">Консоль</button>
    <span class="spacer"></span>
    <span id="cmd-updated"></span>
  </footer>
  <div id="toast" hidden></div>
  <dialog id="settings-dialog">
    <form id="settings-form">
      <input id="settings-old" type="password" />
      <input id="settings-new" type="password" />
      <input id="settings-new2" type="password" />
      <p id="settings-error" hidden></p>
      <code id="settings-data-dir"></code>
      <button type="submit" id="settings-submit">Сменить пароль</button>
      <button type="button" id="settings-close">Закрыть</button>
    </form>
  </dialog>
  <dialog id="creds-dialog">
    <form id="creds-form">
      <span id="creds-bottle-id"></span>
      <input id="creds-username" />
      <input id="creds-password" type="password" />
      <p id="creds-error" hidden></p>
      <button type="submit" id="creds-submit">Сохранить</button>
      <button type="button" id="creds-cancel">Отмена</button>
    </form>
  </dialog>
</main>`;

/** Заглушка GuiApi: каждый метод — vi.fn с рабочим дефолтом. */
function makeGuiApi(overrides: Partial<GuiApi> = {}): GuiApi {
  return {
    listBottles: vi.fn(async () => ROWS.map((row) => ({ ...row }))),
    daemonConnect: vi.fn(async () => {}),
    startBottle: vi.fn(async () => {}),
    stopBottle: vi.fn(async () => {}),
    stopDaemon: vi.fn(async () => {}),
    daemonStats: vi.fn(async () => ({ ...STATS })),
    guiLogRecent: vi.fn(async () => []),
    logsDir: vi.fn(async () => "/tmp/pogrebok-logs"),
    changePassword: vi.fn(async () => null),
    dataDir: vi.fn(async () => "/tmp/pogrebok-data"),
    getSetting: vi.fn(async () => null),
    setSetting: vi.fn(async () => {}),
    applyWindowSize: vi.fn(async () => {}),
    updateCredentials: vi.fn(async () => {}),
    logEvent: vi.fn(async () => {}),
    openEditor: vi.fn(async () => {}),
    getBottle: vi.fn(async () => null),
    saveBottle: vi.fn(async () => {}),
    deleteBottle: vi.fn(async () => {}),
    ...overrides,
  };
}

/** Свежие модули + чистый DOM: module-level состояние каркаса не течёт
 * между тестами. */
async function boot() {
  vi.resetModules();
  document.body.innerHTML = HTML;
  const app = await import("./app");
  const backend = await import("./backend");
  const ipc = await import("./ipc");
  return { app, backend, ipc };
}

/** Ждёт, пока условие не станет истиной (асинхронные цепочки mountApp). */
async function until(check: () => void): Promise<void> {
  await vi.waitFor(check, { timeout: 1000, interval: 10 });
}

function gridRow(id: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(`[data-bottle-id="${id}"]`);
}

function gridIds(): string[] {
  return [...document.querySelectorAll<HTMLElement>("[data-bottle-id]")].map(
    (row) => row.dataset.bottleId ?? "",
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("главное окно 0.3", () => {
  it("рендер без демона: список из БД, кнопки погашены, панель предлагает запуск", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi({
      daemonConnect: vi.fn(async () => {
        throw new Error(DAEMON_DOWN);
      }),
    });
    backend.setGuiApi(api);
    app.mountApp({ once: true });

    await until(() => {
      expect(gridIds().sort()).toEqual(["office-a", "office-b"]);
    });
    // Мета-строка честно сообщает, что демон выключен (пункт 6: список
    // показывается и без демона).
    expect(gridRow("office-a")?.querySelector(".grid-meta")?.textContent).toBe(
      "демон выкл.",
    );
    const action = gridRow("office-a")?.querySelector<HTMLButtonElement>(
      "[data-role=action]",
    );
    expect(action?.disabled).toBe(true);
    expect(action?.classList.contains("off")).toBe(true);
    expect(action?.title).toBe("демон не запущен");
    // Пилюля без состояния.
    expect(
      gridRow("office-a")?.querySelector("[data-role=state]")?.textContent,
    ).toBe("—");
    // Командная панель: демон остановлен, видна кнопка запуска.
    expect(document.querySelector("#cmd-status")?.textContent).toContain(
      "демон остановлен",
    );
    expect(
      document.querySelector<HTMLElement>("#cmd-daemon-start")?.hidden,
    ).toBe(false);
    expect(
      document.querySelector<HTMLElement>("#cmd-daemon-stop")?.hidden,
    ).toBe(true);
  });

  it("счётчики вкладок: Env · 2 / Host · 1", async () => {
    const { app, backend } = await boot();
    backend.setGuiApi(makeGuiApi());
    app.mountApp({ once: true });

    await until(() => {
      expect(
        document.querySelector<HTMLButtonElement>('[data-screen="env"]')
          ?.textContent,
      ).toBe("Env · 2");
    });
    expect(
      document.querySelector<HTMLButtonElement>('[data-screen="host"]')
        ?.textContent,
    ).toBe("Host · 1");
  });

  it("вкладка Host фильтрует сетку до host-бутылок", async () => {
    const { app, backend } = await boot();
    backend.setGuiApi(makeGuiApi());
    app.mountApp({ once: true });

    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });
    document.querySelector<HTMLButtonElement>('[data-screen="host"]')?.click();
    expect(gridIds()).toEqual(["legacy-ac"]);
    expect(
      document.querySelector<HTMLButtonElement>('[data-screen="host"]')
        ?.classList.contains("active"),
    ).toBe(true);
    // Обратно на Env — обе env-бутылки на месте.
    document.querySelector<HTMLButtonElement>('[data-screen="env"]')?.click();
    expect(gridIds().sort()).toEqual(["office-a", "office-b"]);
  });

  it("StateChanged обновляет строку и счётчик активных в панели", async () => {
    const { app, backend } = await boot();
    backend.setGuiApi(makeGuiApi());
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    app.handleDaemonEvent({
      StateChanged: {
        id: "office-a",
        state: "up",
        detail: null,
        socks5: 55123,
        forwards: [{ port: 13389, target: "10.20.30.15:3389" }],
      },
    });
    const pill = gridRow("office-a")?.querySelector("[data-role=state]");
    expect(pill?.textContent).toBe("up");
    expect(pill?.className).toBe("pill pill-up");
    expect(gridRow("office-a")?.querySelector(".grid-meta")?.textContent)
      .toContain("socks5 :55123");
    expect(document.querySelector("#cmd-status")?.textContent).toContain(
      "активных: 1",
    );

    // degraded с причиной: текст причины — в title пилюли.
    app.handleDaemonEvent({
      StateChanged: {
        id: "office-b",
        state: "degraded",
        detail: "handshake не проходит 94 с",
        socks5: null,
        forwards: [],
      },
    });
    const degradedPill = gridRow("office-b")?.querySelector(
      "[data-role=state]",
    );
    expect(degradedPill?.textContent).toBe("degraded");
    expect(degradedPill?.getAttribute("title")).toBe(
      "handshake не проходит 94 с",
    );
  });

  it("разрыв event-соединения гасит сетку до переподключения", async () => {
    const { app, backend } = await boot();
    backend.setGuiApi(makeGuiApi());
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });
    app.handleDaemonEvent({
      StateChanged: {
        id: "office-a",
        state: "up",
        detail: null,
        socks5: 55123,
        forwards: [],
      },
    });
    expect(app.isDaemonDownNow()).toBe(false);

    app.handleDaemonDisconnected();
    expect(app.isDaemonDownNow()).toBe(true);
    expect(gridRow("office-a")?.querySelector(".grid-meta")?.textContent).toBe(
      "демон выкл.",
    );
    expect(document.querySelector("#cmd-status")?.textContent).toContain(
      "демон остановлен",
    );
  });

  it("watchdog: отказ stats → down; восстановление → повторный connect", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi({
      daemonStats: vi
        .fn(async () => ({ ...STATS }))
        .mockRejectedValueOnce(new Error(DAEMON_DOWN)),
    });
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    await app.daemonWatchdog();
    expect(app.isDaemonDownNow()).toBe(true);

    await app.daemonWatchdog();
    expect(app.isDaemonDownNow()).toBe(false);
    // 1 раз при монтировании + 1 раз при восстановлении watchdog'ом.
    expect(api.daemonConnect).toHaveBeenCalledTimes(2);
  });

  it("старт/стоп идут через мост; ошибка демона — тостом дословно", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi({
      stopBottle: vi.fn(async () => {
        throw new Error("бутылка занята: идёт проброс портов");
      }),
    });
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    await app.performBottleAction("office-a", "start");
    expect(api.startBottle).toHaveBeenCalledWith("office-a");

    await app.performBottleAction("office-a", "stop");
    const toast = document.querySelector<HTMLElement>("#toast");
    expect(toast?.hidden).toBe(false);
    expect(toast?.textContent).toBe("бутылка занята: идёт проброс портов");
  });

  it("«Логин и пароль»: модальный диалог сохраняет креды через мост", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    // Кнопка активна только у openvpn-бутылки.
    const credsButton = gridRow("office-b")?.querySelector<HTMLButtonElement>(
      "[data-role=creds]",
    );
    expect(credsButton?.disabled).toBe(false);
    credsButton?.click();

    const dialog = document.querySelector<HTMLDialogElement>("#creds-dialog");
    await until(() => {
      expect(dialog?.open).toBe(true);
    });
    expect(document.querySelector("#creds-bottle-id")?.textContent).toBe(
      "office-b",
    );
    document.querySelector<HTMLInputElement>("#creds-username")!.value =
      "vpn-user";
    document.querySelector<HTMLInputElement>("#creds-password")!.value =
      "s3cret";
    document
      .querySelector<HTMLFormElement>("#creds-form")
      ?.dispatchEvent(new Event("submit", { cancelable: true }));

    await until(() => {
      expect(api.updateCredentials).toHaveBeenCalledWith(
        "office-b",
        "vpn-user",
        "s3cret",
      );
    });
    expect(dialog?.open).toBe(false);
  });

  it("«Запустить демона»: спавн pogrebok-daemon, затем подключение и sync", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi({
      daemonConnect: vi
        .fn(async () => {})
        .mockRejectedValueOnce(new Error(DAEMON_DOWN)),
    });
    backend.setGuiApi(api);
    const start = vi.fn(async () => {});
    app.setDaemonApi({
      start,
      status: vi.fn(async () => ({ running: false, log_tail: "" })),
    });
    app.mountApp({ once: true });
    await until(() => {
      expect(
        document.querySelector<HTMLElement>("#cmd-daemon-start")?.hidden,
      ).toBe(false);
    });

    document.querySelector<HTMLButtonElement>("#cmd-daemon-start")?.click();
    await until(() => {
      expect(start).toHaveBeenCalledTimes(1);
      expect(api.daemonConnect).toHaveBeenCalledTimes(2);
    });
    await until(() => {
      expect(document.querySelector("#cmd-status")?.textContent).toContain(
        "демон работает",
      );
    });
  });

  it("«Остановить демона»: с подтверждением шлёт Shutdown", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    app.setConfirmAction(() => true);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    document.querySelector<HTMLButtonElement>("#cmd-daemon-stop")?.click();
    await until(() => {
      expect(api.stopDaemon).toHaveBeenCalledTimes(1);
    });
    // Отмена подтверждения — Shutdown не уходит.
    app.setConfirmAction(() => false);
    document.querySelector<HTMLButtonElement>("#cmd-daemon-stop")?.click();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(api.stopDaemon).toHaveBeenCalledTimes(1);
  });

  it("«Консоль» открывает вкладку Диагностики", async () => {
    const { app, backend, ipc } = await boot();
    backend.setGuiApi(makeGuiApi());
    ipc.setBottleInvoker(async () => ({
      stdout: JSON.stringify({ DoctorReport: { items: [] } }),
      code: 0,
    }));
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    document.querySelector<HTMLButtonElement>("#cmd-console")?.click();
    expect(
      document
        .querySelector<HTMLButtonElement>('[data-screen="doctor"]')
        ?.classList.contains("active"),
    ).toBe(true);
    expect(
      document.querySelector<HTMLElement>("#screen-doctor")?.hidden,
    ).toBe(false);
    expect(
      document.querySelector<HTMLElement>("#screen-bottles")?.hidden,
    ).toBe(true);
  });

  it("вкладка Диагностики: консоль монтируется и гаснет при уходе", async () => {
    const { app, backend, ipc } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    ipc.setBottleInvoker(async () => ({
      stdout: JSON.stringify({ DoctorReport: { items: [] } }),
      code: 0,
    }));
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    document
      .querySelector<HTMLButtonElement>('[data-screen="doctor"]')
      ?.click();
    await until(() => {
      expect(
        document.querySelectorAll("#stats-cards .stat-card").length,
      ).toBeGreaterThan(0);
    });
    expect(api.daemonStats).toHaveBeenCalled();
    expect(api.guiLogRecent).toHaveBeenCalled();

    // Уход на Env — консоль уничтожена, stats больше не опрашивается.
    const calls = (api.daemonStats as ReturnType<typeof vi.fn>).mock.calls
      .length;
    document.querySelector<HTMLButtonElement>('[data-screen="env"]')?.click();
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(
      (api.daemonStats as ReturnType<typeof vi.fn>).mock.calls.length,
    ).toBe(calls);
  });

  it("Ctrl+, открывает настройки; F5 обновляет список и статусы", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: ",", code: "Comma", ctrlKey: true }),
    );
    expect(
      document.querySelector<HTMLDialogElement>("#settings-dialog")?.open,
    ).toBe(true);

    const listCalls = (api.listBottles as ReturnType<typeof vi.fn>).mock.calls
      .length;
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "F5" }));
    await until(() => {
      expect(
        (api.listBottles as ReturnType<typeof vi.fn>).mock.calls.length,
      ).toBeGreaterThan(listCalls);
    });
    expect(api.daemonStats).toHaveBeenCalled();
  });

  it("вкладка восстанавливается из ui.tab и сохраняется при переключении", async () => {
    const { app, backend } = await boot();
    const store: Record<string, string> = { "ui.tab": "host" };
    const api = makeGuiApi({
      getSetting: vi.fn(async (key: string) => store[key] ?? null),
      setSetting: vi.fn(async (key: string, value: string) => {
        store[key] = value;
      }),
    });
    backend.setGuiApi(api);
    app.mountApp({ once: true });

    // Восстановленная вкладка Host фильтрует сетку до host-бутылок.
    await until(() => {
      expect(gridIds()).toEqual(["legacy-ac"]);
    });
    expect(
      document
        .querySelector<HTMLButtonElement>('[data-screen="host"]')
        ?.classList.contains("active"),
    ).toBe(true);

    // Переключение вкладки перезаписывает настройку.
    document.querySelector<HTMLButtonElement>('[data-screen="env"]')?.click();
    await until(() => {
      expect(store["ui.tab"]).toBe("env");
    });
  });

  it("ошибка БД при list — тостом, сетка пустая", async () => {
    const { app, backend } = await boot();
    backend.setGuiApi(
      makeGuiApi({
        listBottles: vi.fn(async () => {
          throw new Error("хранилище не разблокировано");
        }),
      }),
    );
    app.mountApp({ once: true });
    await until(() => {
      expect(document.querySelector("#toast")?.textContent).toBe(
        "хранилище не разблокировано",
      );
    });
    expect(gridIds()).toHaveLength(0);
    expect(
      document.querySelector<HTMLElement>("#grid-empty")?.hidden,
    ).toBe(false);
  });

  it("«+ Создать бутылку» и «Изменить» открывают редактор через мост", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });

    document.querySelector<HTMLButtonElement>("#create-open")?.click();
    await until(() => {
      expect(api.openEditor).toHaveBeenCalledWith(null, null);
    });

    gridRow("office-a")
      ?.querySelector<HTMLButtonElement>("[data-role=edit]")
      ?.click();
    await until(() => {
      expect(api.openEditor).toHaveBeenCalledWith("office-a", "stopped");
    });

    // Поднятая бутылка передаёт состояние (баннер/запрет удаления в окне).
    app.handleDaemonEvent({
      StateChanged: {
        id: "office-a",
        state: "up",
        detail: null,
        socks5: null,
        forwards: [],
      },
    });
    gridRow("office-a")
      ?.querySelector<HTMLButtonElement>("[data-role=edit]")
      ?.click();
    await until(() => {
      expect(api.openEditor).toHaveBeenCalledWith("office-a", "up");
    });
  });

  it("bottles-changed из окна редактора: тост и обновление сетки", async () => {
    const { app, backend } = await boot();
    const api = makeGuiApi();
    backend.setGuiApi(api);
    app.mountApp({ once: true });
    await until(() => {
      expect(gridIds()).toHaveLength(2);
    });
    const callsBefore = (api.listBottles as ReturnType<typeof vi.fn>).mock
      .calls.length;

    backend.dispatchBottlesChanged({ action: "created", id: "office-c" });
    await until(() => {
      expect(document.querySelector("#toast")?.textContent).toBe(
        "Бутылка office-c создана",
      );
    });
    await until(() => {
      expect(
        (api.listBottles as ReturnType<typeof vi.fn>).mock.calls.length,
      ).toBeGreaterThan(callsBefore);
    });
  });
});
