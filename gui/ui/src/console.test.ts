// Тесты консоли Диагностики (Task 17, бриф Step 1): фильтры по
// уровню/источнику/категории, пауза без потери событий, поиск с
// дебаунсом, обрезка буфера на 5000, слияние gui+daemon лент по ts.

import { afterEach, describe, expect, it, vi } from "vitest";
import type { DaemonStatsWire, EventWire } from "./ipc";
import {
  EventBuffer,
  applyFilters,
  categoryOf,
  defaultFilters,
  formatUptime,
  mergeFeeds,
  mountConsole,
  type ConsoleDeps,
} from "./console";

const event = (
  ts: string,
  code: string,
  level: EventWire["level"] = "info",
  src: EventWire["src"] = "daemon",
  desc = "",
): EventWire => ({ ts, level, src, event: code, desc, ctx: {} });

const STATS: DaemonStatsWire = {
  pid: 4242,
  version: "0.3.0-test",
  started_at: new Date(Date.now() - 3_723_000).toISOString(), // 1 ч 2 мин
  active_bottles: 2,
  total_profiles: 4,
  mem_bytes: 48 * 1024 * 1024,
  cpu_pct: 1.7,
  wsl_state: "running",
  appliance_version: "0.3.0",
};

const HTML = `
<div id="screen-doctor">
  <div id="stats-cards"></div>
  <div class="chips" id="chips-src"></div>
  <div class="chips" id="chips-level"></div>
  <div class="chips" id="chips-cat"></div>
  <input id="console-search" />
  <button id="console-pause" type="button">Пауза</button>
  <div id="console-list"></div>
  <span id="console-count"></span>
</div>`;

interface Push {
  (e: EventWire): void;
}

function makeDeps(overrides: Partial<ConsoleDeps> = {}): {
  deps: ConsoleDeps;
  push: Push;
  stats: ReturnType<typeof vi.fn>;
} {
  const listeners: ((e: EventWire) => void)[] = [];
  const stats = vi.fn(async () => ({ ...STATS }));
  return {
    deps: {
      daemonFeed: () => [],
      guiRecent: async () => [],
      onDaemonEvent: (fn) => {
        listeners.push(fn);
        return () => {
          const index = listeners.indexOf(fn);
          if (index !== -1) {
            listeners.splice(index, 1);
          }
        };
      },
      stats,
      ...overrides,
    },
    push: (e) => {
      for (const fn of [...listeners]) {
        fn(e);
      }
    },
    stats,
  };
}

async function until(check: () => void): Promise<void> {
  await vi.waitFor(check, { timeout: 1500, interval: 15 });
}

function listEvents(): string[] {
  return [...document.querySelectorAll(".console-row .c-event")].map(
    (node) => node.textContent ?? "",
  );
}

function chip(container: string, value: string): HTMLButtonElement | null {
  return [
    ...document.querySelectorAll<HTMLButtonElement>(`${container} .chip`),
  ].find((node) => node.dataset.value === value || node.textContent === value) ?? null;
}

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("чистые функции консоли", () => {
  it("categoryOf: префикс до первой точки", () => {
    expect(categoryOf("bottle.starting")).toBe("bottle");
    expect(categoryOf("daemon.shutdown")).toBe("daemon");
    expect(categoryOf("profiles.synced.now")).toBe("profiles");
    expect(categoryOf("nodots")).toBe("nodots");
  });

  it("mergeFeeds: gui+daemon в одну ленту по ts, равные ts стабильны", () => {
    const gui = [
      event("2026-10-22T10:00:02Z", "gui.b", "info", "gui"),
      event("2026-10-22T10:00:04Z", "gui.d", "info", "gui"),
    ];
    const daemon = [
      event("2026-10-22T10:00:01Z", "daemon.a"),
      event("2026-10-22T10:00:03Z", "daemon.c"),
    ];
    expect(mergeFeeds(daemon, gui).map((e) => e.event)).toEqual([
      "daemon.a",
      "gui.b",
      "daemon.c",
      "gui.d",
    ]);
  });

  it("applyFilters: уровень, источник, категория, поиск", () => {
    const events = [
      event("2026-10-22T10:00:01Z", "bottle.up", "info", "daemon", "office-a поднята"),
      event("2026-10-22T10:00:02Z", "bottle.failed", "error", "daemon", "office-b упала"),
      event("2026-10-22T10:00:03Z", "db.unlocked", "info", "gui", "хранилище открыто"),
    ];
    const filters = defaultFilters();
    // Уровень.
    filters.levels.delete("info");
    expect(applyFilters(events, filters).map((e) => e.event)).toEqual([
      "bottle.failed",
    ]);
    // Источник.
    const bySrc = defaultFilters();
    bySrc.src.delete("daemon");
    expect(applyFilters(events, bySrc).map((e) => e.event)).toEqual([
      "db.unlocked",
    ]);
    // Категория.
    const byCat = defaultFilters();
    byCat.categories = new Set(["bottle"]);
    expect(applyFilters(events, byCat).map((e) => e.event)).toEqual([
      "bottle.up",
      "bottle.failed",
    ]);
    // Поиск по описанию без учёта регистра.
    const bySearch = defaultFilters();
    bySearch.search = "OFFICE-B";
    expect(applyFilters(events, bySearch).map((e) => e.event)).toEqual([
      "bottle.failed",
    ]);
  });

  it("EventBuffer: обрезается на cap, старшие вытесняются", () => {
    const buffer = new EventBuffer(5000);
    for (let index = 0; index < 5010; index += 1) {
      buffer.push(event("2026-10-22T10:00:00Z", `e.${index}`));
    }
    expect(buffer.size).toBe(5000);
    const all = buffer.all();
    expect(all[0]?.event).toBe("e.10");
    expect(all[4999]?.event).toBe("e.5009");
  });

  it("formatUptime: секунды, минуты, часы, дни", () => {
    const now = new Date("2026-10-22T12:00:00Z");
    expect(formatUptime("2026-10-22T11:59:40Z", now)).toBe("20 с");
    expect(formatUptime("2026-10-22T11:55:00Z", now)).toBe("5 мин");
    expect(formatUptime("2026-10-22T09:46:00Z", now)).toBe("2 ч 14 мин");
    expect(formatUptime("2026-10-21T09:00:00Z", now)).toBe("1 д 3 ч");
    expect(formatUptime("не-дата", now)).toBe("—");
  });
});

describe("живая консоль", () => {
  it("seed: ленты демона и GUI сливаются и рендерятся", async () => {
    document.body.innerHTML = HTML;
    const { deps } = makeDeps({
      daemonFeed: () => [event("2026-10-22T10:00:02Z", "bottle.up")],
      guiRecent: async () => [
        event("2026-10-22T10:00:01Z", "db.unlocked", "info", "gui"),
      ],
    });
    const root = document.querySelector<HTMLElement>("#screen-doctor");
    const handle = mountConsole(root!, deps);
    await until(() => {
      expect(listEvents()).toEqual(["db.unlocked", "bottle.up"]);
    });
    handle.destroy();
  });

  it("push-события появляются в ленте; счётчик обновляется", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.starting"));
    push(event("2026-10-22T10:00:02Z", "bottle.up"));
    await until(() => {
      expect(listEvents()).toEqual(["bottle.starting", "bottle.up"]);
    });
    expect(document.querySelector("#console-count")?.textContent).toContain(
      "строк: 2 из 2",
    );
    handle.destroy();
  });

  it("чипы источника и уровня фильтруют ленту", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.up", "info", "daemon"));
    push(event("2026-10-22T10:00:02Z", "db.unlocked", "info", "gui"));
    push(event("2026-10-22T10:00:03Z", "bottle.failed", "error", "daemon"));
    await until(() => {
      expect(listEvents()).toHaveLength(3);
    });

    chip("#chips-src", "gui")?.click();
    await until(() => {
      expect(listEvents()).toEqual(["bottle.up", "bottle.failed"]);
    });
    chip("#chips-level", "error")?.click();
    await until(() => {
      expect(listEvents()).toEqual(["bottle.up"]);
    });
    handle.destroy();
  });

  it("чипы категорий строятся из ленты и фильтруют", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.up"));
    push(event("2026-10-22T10:00:02Z", "daemon.shutdown"));
    await until(() => {
      expect(listEvents()).toHaveLength(2);
    });
    const daemonChip = chip("#chips-cat", "daemon");
    expect(daemonChip).not.toBeNull();
    expect(chip("#chips-cat", "bottle")).not.toBeNull();
    daemonChip?.click();
    await until(() => {
      expect(listEvents()).toEqual(["bottle.up"]);
    });
    handle.destroy();
  });

  it("пауза не теряет события: после снятия видны все", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.up"));
    await until(() => {
      expect(listEvents()).toEqual(["bottle.up"]);
    });

    document.querySelector<HTMLButtonElement>("#console-pause")?.click();
    expect(document.querySelector("#console-pause")?.textContent).toBe(
      "Продолжить",
    );
    push(event("2026-10-22T10:00:02Z", "bottle.stopping"));
    push(event("2026-10-22T10:00:03Z", "bottle.stopped"));
    // На паузе лента не перерисовывается.
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(listEvents()).toEqual(["bottle.up"]);

    document.querySelector<HTMLButtonElement>("#console-pause")?.click();
    await until(() => {
      expect(listEvents()).toEqual([
        "bottle.up",
        "bottle.stopping",
        "bottle.stopped",
      ]);
    });
    handle.destroy();
  });

  it("поиск с дебаунсом фильтрует по событию и описанию", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.up", "info", "daemon", "office-a поднята"));
    push(event("2026-10-22T10:00:02Z", "bottle.failed", "error", "daemon", "office-b упала"));
    await until(() => {
      expect(listEvents()).toHaveLength(2);
    });
    const search = document.querySelector<HTMLInputElement>("#console-search");
    if (search !== null) {
      search.value = "office-b";
      search.dispatchEvent(new Event("input"));
    }
    // Дебаунс 150 мс — раньше времени фильтра нет.
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(listEvents()).toHaveLength(2);
    await until(() => {
      expect(listEvents()).toEqual(["bottle.failed"]);
    });
    handle.destroy();
  });

  it("статус-карты из daemon_stats; недоступный демон — карта «остановлен»", async () => {
    document.body.innerHTML = HTML;
    const { deps } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    await until(() => {
      expect(document.querySelector("#stats-cards")?.textContent).toContain(
        "pid 4242",
      );
    });
    const text = document.querySelector("#stats-cards")?.textContent ?? "";
    expect(text).toContain("1 ч 2 мин");
    expect(text).toContain("2 активных");
    expect(text).toContain("48 МБ");
    expect(text).toContain("running");
    expect(text).toContain("апплайенс 0.3.0");
    handle.destroy();

    document.body.innerHTML = HTML;
    const down = makeDeps({
      stats: vi.fn(async () => {
        throw new Error("не удалось подключиться к демону");
      }),
    });
    const handle2 = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      down.deps,
    );
    await until(() => {
      expect(document.querySelector("#stats-cards")?.textContent).toContain(
        "остановлен",
      );
    });
    handle2.destroy();
  });

  it("destroy: события после уничтожения не рендерятся", async () => {
    document.body.innerHTML = HTML;
    const { deps, push } = makeDeps();
    const handle = mountConsole(
      document.querySelector<HTMLElement>("#screen-doctor")!,
      deps,
    );
    push(event("2026-10-22T10:00:01Z", "bottle.up"));
    await until(() => {
      expect(listEvents()).toEqual(["bottle.up"]);
    });
    handle.destroy();
    push(event("2026-10-22T10:00:02Z", "bottle.stopped"));
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(listEvents()).toEqual(["bottle.up"]);
  });
});
