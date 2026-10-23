// Живая консоль и статус-карты вкладки «Диагностика» (Task 17,
// спека §3.4): единая лента событий демона и GUI (слияние по ts),
// фильтры-чипы (источник, уровень, категория), поиск с дебаунсом,
// пауза без потери событий, автопрокрутка; окно рендера — 200 строк,
// буфер — 5000. Карты: демон (pid, версия), аптайм, бутылки, память/CPU,
// WSL — обновляются по таймеру из daemon_stats. Зависимости приходят
// параметром (app.ts), чтобы модуль не тянул каркас и оставался
// тестируемым.

import type { DaemonStatsWire, EventWire } from "./ipc";

/** Максимум событий в буфере; старше — вытесняются (бриф Step 1). */
export const BUFFER_CAP = 5000;
/** Сколько строк рендерится (виртуальное окно с хвоста ленты). */
export const WINDOW_ROWS = 200;
/** Дебаунс поиска, мс. */
export const SEARCH_DEBOUNCE_MS = 150;
/** Период обновления статус-карт, мс. */
export const STATS_REFRESH_MS = 5000;

/** Категория события — префикс кода до первой точки («bottle.up» → «bottle»). */
export function categoryOf(event: string): string {
  const dot = event.indexOf(".");
  return dot === -1 ? event : event.slice(0, dot);
}

/** Слияние лент в одну по ts (стабильная сортировка: равные ts
 * сохраняют порядок вставки). */
export function mergeFeeds(...feeds: EventWire[][]): EventWire[] {
  const all = feeds.flat();
  all.sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0));
  return all;
}

export interface ConsoleFilters {
  src: Set<string>;
  levels: Set<string>;
  /** Включённые категории (динамические; новые добавляются включёнными). */
  categories: Set<string>;
  search: string;
}

/** Все источники и уровни по умолчанию включены. */
export function defaultFilters(): ConsoleFilters {
  return {
    src: new Set(["daemon", "gui", "svc"]),
    levels: new Set(["debug", "info", "warn", "error", "fatal"]),
    categories: new Set(),
    search: "",
  };
}

/** Фильтрация ленты: источник, уровень, категория (пустой набор — все),
 * поиск по событию/описанию/источнику без учёта регистра. */
export function applyFilters(
  events: EventWire[],
  filters: ConsoleFilters,
): EventWire[] {
  const search = filters.search.trim().toLowerCase();
  return events.filter((event) => {
    if (!filters.src.has(event.src) || !filters.levels.has(event.level)) {
      return false;
    }
    if (
      filters.categories.size > 0 &&
      !filters.categories.has(categoryOf(event.event))
    ) {
      return false;
    }
    if (search === "") {
      return true;
    }
    return (
      event.event.toLowerCase().includes(search) ||
      event.desc.toLowerCase().includes(search) ||
      event.src.toLowerCase().includes(search)
    );
  });
}

/** Буфер ленты с вытеснением старших (cap). */
export class EventBuffer {
  private events: EventWire[] = [];

  constructor(private readonly cap: number = BUFFER_CAP) {}

  seed(events: EventWire[]): void {
    this.events = events.slice(-this.cap);
  }

  push(event: EventWire): void {
    this.events.push(event);
    if (this.events.length > this.cap) {
      this.events.splice(0, this.events.length - this.cap);
    }
  }

  all(): EventWire[] {
    return [...this.events];
  }

  get size(): number {
    return this.events.length;
  }
}

/** Аптайм из started_at: «5 с», «3 мин», «2 ч 14 мин», «1 д 3 ч». */
export function formatUptime(startedAt: string, now: Date): string {
  const started = Date.parse(startedAt);
  if (Number.isNaN(started)) {
    return "—";
  }
  const seconds = Math.max(0, Math.floor((now.getTime() - started) / 1000));
  if (seconds < 60) {
    return `${seconds} с`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes} мин`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} ч ${minutes % 60} мин`;
  }
  return `${Math.floor(hours / 24)} д ${hours % 24} ч`;
}

/** Зависимости консоли (собирает app.ts из своего состояния и GuiApi). */
export interface ConsoleDeps {
  /** Текущий буфер событий демона (push-ленты app.ts). */
  daemonFeed(): EventWire[];
  /** Последние события журнала GUI (gui_log_recent). */
  guiRecent(): Promise<EventWire[]>;
  /** Подписка на живые события демона. Возвращает отписку. */
  onDaemonEvent(fn: (event: EventWire) => void): () => void;
  /** daemon_stats; throw — демон недоступен. */
  stats(): Promise<DaemonStatsWire>;
}

export interface ConsoleHandle {
  destroy(): void;
}

const SRC_LABELS: Record<string, string> = {
  daemon: "демон",
  gui: "gui",
  svc: "служба",
};

/**
 * Монтирует консоль и статус-карты внутри #screen-doctor: чипы, поиск,
 * пауза, лента (окно 200 строк, рендер батчами по rAF), карты по
 * таймеру. destroy() снимает таймеры и подписки (уход со вкладки).
 */
export function mountConsole(
  root: HTMLElement,
  deps: ConsoleDeps,
): ConsoleHandle {
  const q = <T extends HTMLElement>(selector: string): T | null =>
    root.querySelector<T>(selector);
  const list = q("#console-list");
  const counter = q("#console-count");
  const pauseButton = q<HTMLButtonElement>("#console-pause");
  const searchInput = q<HTMLInputElement>("#console-search");
  const cards = q("#stats-cards");

  const buffer = new EventBuffer();
  const filters = defaultFilters();
  let paused = false;
  let dirty = false;
  let destroyed = false;
  let rafId = 0;

  // --- Чипы фильтров --------------------------------------------------------

  const buildChips = (
    container: HTMLElement | null,
    values: readonly string[],
    labels: Record<string, string>,
    selected: Set<string>,
  ): void => {
    if (container === null) {
      return;
    }
    container.innerHTML = "";
    for (const value of values) {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = `chip${selected.has(value) ? " on" : ""}`;
      chip.textContent = labels[value] ?? value;
      chip.dataset.value = value;
      chip.addEventListener("click", () => {
        if (selected.has(value)) {
          selected.delete(value);
        } else {
          selected.add(value);
        }
        chip.classList.toggle("on", selected.has(value));
        scheduleRender();
      });
      container.append(chip);
    }
  };

  const rebuildCategoryChips = (): void => {
    const cats = new Set<string>();
    for (const event of buffer.all()) {
      cats.add(categoryOf(event.event));
    }
    const container = q("#chips-cat");
    if (container === null) {
      return;
    }
    container.innerHTML = "";
    for (const cat of [...cats].sort()) {
      // Пустой набор фильтра = режим «все категории».
      const on = filters.categories.size === 0 || filters.categories.has(cat);
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = `chip${on ? " on" : ""}`;
      chip.textContent = cat;
      chip.dataset.value = cat;
      chip.addEventListener("click", () => {
        if (filters.categories.size === 0) {
          // Из режима «все» — выключить выбранную.
          filters.categories = new Set([...cats].filter((c) => c !== cat));
        } else if (filters.categories.has(cat)) {
          filters.categories.delete(cat);
        } else {
          filters.categories.add(cat);
        }
        // Все включены снова — возвращаем режим «все».
        if (filters.categories.size === cats.size) {
          filters.categories = new Set();
        }
        rebuildCategoryChips();
        scheduleRender();
      });
      container.append(chip);
    }
  };

  buildChips(q("#chips-src"), ["daemon", "gui", "svc"], SRC_LABELS, filters.src);
  buildChips(
    q("#chips-level"),
    ["debug", "info", "warn", "error", "fatal"],
    {},
    filters.levels,
  );

  // --- Лента ----------------------------------------------------------------

  const renderRows = (): void => {
    if (list === null) {
      return;
    }
    const filtered = applyFilters(buffer.all(), filters);
    const visible = filtered.slice(-WINDOW_ROWS);
    list.innerHTML = "";
    for (const event of visible) {
      const row = document.createElement("div");
      row.className = `console-row lvl-${event.level}`;
      const time = document.createElement("span");
      time.className = "c-time";
      const parsed = new Date(event.ts);
      time.textContent = Number.isNaN(parsed.getTime())
        ? event.ts
        : parsed.toLocaleTimeString("ru-RU");
      const level = document.createElement("span");
      level.className = `c-level lv-${event.level}`;
      level.textContent = event.level;
      const src = document.createElement("span");
      src.className = "c-src";
      src.textContent = SRC_LABELS[event.src] ?? event.src;
      const code = document.createElement("span");
      code.className = "c-event";
      code.textContent = event.event;
      const desc = document.createElement("span");
      desc.className = "c-desc";
      desc.textContent = event.desc;
      row.append(time, level, src, code, desc);
      list.append(row);
    }
    // Автопрокрутка к последнему событию (спека §3.4).
    list.scrollTop = list.scrollHeight;
    if (counter !== null) {
      const pending = paused ? " · на паузе" : "";
      counter.textContent = `строк: ${visible.length} из ${filtered.length}${pending}`;
    }
  };

  const scheduleRender = (): void => {
    if (destroyed || paused || dirty) {
      return;
    }
    dirty = true;
    rafId = requestAnimationFrame(() => {
      dirty = false;
      if (!destroyed && !paused) {
        renderRows();
      }
    });
  };

  pauseButton?.addEventListener("click", () => {
    paused = !paused;
    if (pauseButton !== null) {
      pauseButton.textContent = paused ? "Продолжить" : "Пауза";
      pauseButton.classList.toggle("on", paused);
    }
    if (!paused) {
      // Догон: всё накопленное за паузу выводится разом (бриф Step 1).
      renderRows();
    } else if (counter !== null) {
      counter.textContent += " · на паузе";
    }
  });

  let searchTimer = 0;
  searchInput?.addEventListener("input", () => {
    window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => {
      filters.search = searchInput.value;
      scheduleRender();
    }, SEARCH_DEBOUNCE_MS);
  });

  // --- Статус-карты ---------------------------------------------------------

  const renderStats = (stats: DaemonStatsWire | null): void => {
    if (cards === null) {
      return;
    }
    cards.innerHTML = "";
    const card = (title: string, value: string, hint = ""): void => {
      const element = document.createElement("div");
      element.className = "stat-card";
      const head = document.createElement("span");
      head.className = "stat-title";
      head.textContent = title;
      const main = document.createElement("span");
      main.className = "stat-value";
      main.textContent = value;
      element.append(head, main);
      if (hint !== "") {
        const sub = document.createElement("span");
        sub.className = "stat-hint";
        sub.textContent = hint;
        element.append(sub);
      }
      cards.append(element);
    };
    if (stats === null) {
      card("Демон", "остановлен", "карты обновятся после запуска");
      return;
    }
    card("Демон", `pid ${stats.pid}`, `версия ${stats.version}`);
    card("Аптайм", formatUptime(stats.started_at, new Date()));
    card(
      "Бутылки",
      `${stats.active_bottles} активных`,
      `всего профилей: ${stats.total_profiles}`,
    );
    card(
      "Память / CPU",
      `${Math.round(stats.mem_bytes / 1024 / 1024)} МБ`,
      `${stats.cpu_pct.toFixed(1)} % CPU`,
    );
    card(
      "WSL",
      stats.wsl_state,
      stats.appliance_version !== null
        ? `апплайенс ${stats.appliance_version}`
        : "версия апплайенса неизвестна",
    );
  };

  const refreshStats = async (): Promise<void> => {
    try {
      renderStats(await deps.stats());
    } catch {
      renderStats(null);
    }
  };
  const statsTimer = window.setInterval(() => {
    void refreshStats();
  }, STATS_REFRESH_MS);
  void refreshStats();

  // --- Поток событий ----------------------------------------------------------

  // Снимок ленты демона ДО подписки: живые push'и после этой точки
  // приходят только через onDaemonEvent, иначе seed дал бы дубли.
  const feedSnapshot = deps.daemonFeed();
  const unsubscribe = deps.onDaemonEvent((event) => {
    buffer.push(event);
    rebuildCategoryChips();
    scheduleRender();
  });

  // Первичное наполнение: снимок ленты демона + журнал GUI + всё,
  // что успело прийти живьём, пока guiRecent летел (гонка seed/push).
  void deps.guiRecent().then((guiEvents) => {
    if (destroyed) {
      return;
    }
    buffer.seed(mergeFeeds(feedSnapshot, guiEvents, buffer.all()));
    rebuildCategoryChips();
    scheduleRender();
  });

  return {
    destroy(): void {
      destroyed = true;
      window.clearInterval(statsTimer);
      window.clearTimeout(searchTimer);
      cancelAnimationFrame(rafId);
      unsubscribe();
    },
  };
}
