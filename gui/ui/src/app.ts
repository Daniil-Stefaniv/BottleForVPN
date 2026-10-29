// Каркас приложения 0.3: вкладки Env/Host/Диагностика, сетка бутылок из
// БД (работает без демона), статусы — push-событиями демона (замена
// поллинга; watchdog 30 с ловит тихий разрыв), командная панель демона
// (запуск/остановка/консоль). Список читает `list_bottles` из
// зашифрованного хранилища; старт/стоп идут через DaemonClient.

import { gui, isDaemonDown, setBottlesChangedListener } from "./backend";
import type { DaemonEventWire, EventWire, RuntimeState } from "./ipc";
import {
  mergeListStatus,
  renderBottleGrid,
  type BottleAction,
  type BottleRow,
} from "./list";
import { askCredentials } from "./creds";
import { mountConsole, type ConsoleHandle } from "./console";
import { collectLogs, loadDoctor } from "./doctor";
import {
  installHotkeys,
  installWindowSizePersist,
  persistTab,
  restoreWindowState,
  showSettings,
} from "./settings";

/** API спавна демона (Rust-команда start_daemon), подменяется в тестах. */
export interface DaemonApi {
  start: () => Promise<void>;
  status: () => Promise<{ running: boolean; log_tail: string }>;
}

let daemonApi: DaemonApi | null = null;

export function setDaemonApi(api: DaemonApi): void {
  daemonApi = api;
}

/** Путь к папке логов последнего экспорта (для «Открыть папку»). */
let lastLogsPath: string | null = null;

/** Адаптер «Открыть папку» (Tauri opener; подменяется в тестах). */
let revealInDir: ((path: string) => Promise<void>) | null = null;

export function setRevealInDir(fn: (path: string) => Promise<void>): void {
  revealInDir = fn;
}

/** Интервал watchdog'а демона (замена поллинга статусов, спека §5). */
export const WATCHDOG_MS = 30_000;

export type ToastKind = "error" | "success";

/** Показывает тост (ошибки дословно от демона, успехи — кратко). */
function showToast(message: string, kind: ToastKind = "error"): void {
  const toast = document.querySelector<HTMLElement>("#toast");
  if (toast === null) {
    return;
  }
  toast.textContent = message;
  toast.className = kind === "success" ? "toast-success" : "toast-error";
  toast.hidden = false;
  window.setTimeout(() => {
    toast.hidden = true;
  }, 8000);
}

/** Подписчик на обновления строк сетки (tray-меню). */
let rowsNotifier: ((rows: BottleRow[]) => void) | null = null;

export function setRowsNotifier(fn: ((rows: BottleRow[]) => void) | null): void {
  rowsNotifier = fn;
}

/** Действие, выполняемое прямо сейчас: спиннер на кнопке строки. */
const pendingActions = new Map<string, BottleAction>();

let daemonDown = false;

/** Активная вкладка списка. */
let currentTab: "env" | "host" = "env";

/** Последние строки (для перерисовки при событиях без запроса к БД). */
let rowsCache: BottleRow[] = [];

/** Последние профили из БД (merge с рантаймом при событиях). */
let profilesCache: Parameters<typeof mergeListStatus>[0] = [];

/** Снимок рантайма по id: наполняется push-событиями StateChanged. */
const runtimeCache = new Map<string, RuntimeState>();

/** Лента событий демона для консоли «Диагностики» (Task 17 читает). */
export const daemonLogFeed: EventWire[] = [];

/** Подписчик ленты журнала демона (консоль Диагностики, Task 17). */
let daemonLogListener: ((event: EventWire) => void) | null = null;

export function setDaemonLogListener(fn: ((event: EventWire) => void) | null): void {
  daemonLogListener = fn;
}

/**
 * Id активных бутылок (starting/up/degraded/stopping держат ресурсы) —
 * для текста подтверждения выхода из трея (0.3).
 */
export function activeBottleIds(): string[] {
  const ACTIVE = new Set(["starting", "up", "degraded", "stopping"]);
  return rowsCache.filter((row) => ACTIVE.has(row.state)).map((row) => row.id);
}

/** Демон недоступен? (для тестов и будущих экранов). */
export function isDaemonDownNow(): boolean {
  return daemonDown;
}

/** Перерисовка сетки (фильтр вкладки), счётчиков и командной панели. */
function renderAll(rows: BottleRow[]): void {
  const envCount = rows.filter((row) => row.kind === "env").length;
  const hostCount = rows.length - envCount;
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    if (tab.dataset.screen === "env") {
      tab.textContent = `Env · ${envCount}`;
    } else if (tab.dataset.screen === "host") {
      tab.textContent = `Host · ${hostCount}`;
    }
  }
  const visible = rows.filter((row) => row.kind === currentTab);
  renderBottleGrid(visible, daemonDown, pendingActions, {
    onAction: (id, action) => {
      void performBottleAction(id, action);
    },
    onEdit: (id) => {
      const row = rowsCache.find((item) => item.id === id);
      gui()
        .openEditor(id, row?.state ?? null)
        .catch((error: unknown) => {
          showToast(error instanceof Error ? error.message : String(error));
        });
    },
    onCreds: (id) => {
      openCredsDialog(id);
    },
  });
  updateCmdBar(rows);
}

/**
 * Однократное обновление списка: `list_bottles` из БД + merge с
 * рантайм-кэшем событий. Ошибка «хранилище не разблокировано» —
 * тост; демон для списка не нужен (спека §3).
 */
export async function refreshBottles(): Promise<BottleRow[]> {
  try {
    profilesCache = await gui().listBottles();
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
    rowsCache = [];
    renderAll([]);
    return [];
  }
  return remerge();
}

/** Пересобирает строки из кэшей (профили БД + рантайм) и рисует. */
function remerge(): BottleRow[] {
  const rows = mergeListStatus(profilesCache, [...runtimeCache.values()]);
  rowsCache = rows;
  renderAll(rows);
  rowsNotifier?.(rows);
  return rows;
}

/** Командная панель: статус демона, кнопки запуска/остановки, метка времени. */
function updateCmdBar(rows: BottleRow[]): void {
  const status = document.querySelector<HTMLElement>("#cmd-status");
  if (status !== null) {
    status.innerHTML = "";
    const dot = document.createElement("span");
    dot.className = daemonDown ? "dot dot-failed" : "dot dot-up";
    const active = rows.filter(
      (row) => row.state === "up" || row.state === "degraded",
    ).length;
    status.append(
      dot,
      document.createTextNode(
        daemonDown ? "демон остановлен" : `демон работает · активных: ${active}`,
      ),
    );
  }
  const start = document.querySelector<HTMLElement>("#cmd-daemon-start");
  if (start !== null) {
    start.hidden = !daemonDown;
  }
  const stop = document.querySelector<HTMLElement>("#cmd-daemon-stop");
  if (stop !== null) {
    stop.hidden = daemonDown;
  }
  const updated = document.querySelector<HTMLElement>("#cmd-updated");
  if (updated !== null) {
    updated.textContent = `обновлено ${new Date().toLocaleTimeString("ru-RU")}`;
  }
}

/** Push-событие демона: StateChanged обновляет строку, Log — ленту. */
export function handleDaemonEvent(event: DaemonEventWire): void {
  if ("StateChanged" in event) {
    const change = event.StateChanged;
    runtimeCache.set(change.id, {
      id: change.id,
      state: change.state,
      socks5: change.socks5,
      forwards: change.forwards,
    });
    // Причина failed/degraded лежит в detail — mergeListStatus ждёт
    // проводную форму; прямое присвоение точнее.
    if (change.detail !== null) {
      const live = runtimeCache.get(change.id);
      if (live !== undefined && typeof live.state === "string") {
        if (change.state === "failed") {
          live.state = { Failed: { reason: change.detail } };
        } else if (change.state === "degraded") {
          live.state = { Degraded: { reason: change.detail } };
        }
      }
    }
    remerge();
  } else {
    daemonLogFeed.push(event.Log);
    daemonLogListener?.(event.Log);
  }
}

/** Разрыв event-соединения: статусы неизвестны, watchdog поднимет. */
export function handleDaemonDisconnected(): void {
  if (daemonDown) {
    return;
  }
  daemonDown = true;
  runtimeCache.clear();
  remerge();
}

/**
 * Watchdog (каждые WATCHDOG_MS): тихий разрыв без события ловится
 * отказом daemonStats; поднявшийся демон — переподключением и
 * полной синхронизацией профилей.
 */
export async function daemonWatchdog(): Promise<void> {
  try {
    await gui().daemonStats();
    if (daemonDown) {
      daemonDown = false;
      try {
        await gui().daemonConnect();
      } catch (error) {
        daemonDown = true;
        showToast(error instanceof Error ? error.message : String(error));
      }
    }
    remerge();
  } catch {
    if (!daemonDown) {
      daemonDown = true;
      runtimeCache.clear();
      remerge();
    }
  }
}

/**
 * Старт/стоп бутылки через DaemonClient; на время вызова — спиннер
 * на кнопке строки. Креды демон уже имеет из синхронизации (спека §5) —
 * диалог логина при старте больше не нужен.
 */
export async function performBottleAction(
  id: string,
  action: BottleAction,
): Promise<void> {
  pendingActions.set(id, action);
  renderAll(rowsCache);
  try {
    if (action === "start") {
      await gui().startBottle(id);
    } else {
      await gui().stopBottle(id);
    }
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  } finally {
    pendingActions.delete(id);
    renderAll(rowsCache);
  }
}

/**
 * «Логин и пароль» (пункт 9): модальный диалог обновления кредов
 * бутылки; сохранение — в зашифрованную БД с синхронизацией демону.
 */
function openCredsDialog(id: string): void {
  void askCredentials(id).then(async (creds) => {
    if (creds === null) {
      return;
    }
    try {
      await gui().updateCredentials(id, creds.username, creds.password);
      showToast(`Логин и пароль бутылки ${id} сохранены`, "success");
      await refreshBottles();
    } catch (error) {
      showToast(error instanceof Error ? error.message : String(error));
    }
  });
}

/** «Запустить демона»: спавн pogrebok-daemon, затем подключение и sync. */
async function onStartDaemon(): Promise<void> {
  if (daemonApi === null) {
    return;
  }
  try {
    await daemonApi.start();
    await gui().daemonConnect();
    daemonDown = false;
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  }
  await refreshBottles();
}

/** «Остановить демона»: Shutdown — бутылки останавливаются, демон гаснет. */
async function onDaemonStop(): Promise<void> {
  if (
    !confirmAction(
      "Остановить все бутылки и завершить демона? " +
        "Запустить его снова можно кнопкой «Запустить демона».",
    )
  ) {
    return;
  }
  try {
    await gui().stopDaemon();
    showToast("Демон останавливается", "success");
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  }
}

/** Переключение экранов: активный таб + видимость секций. */
export function switchScreen(screen: string): void {
  if (screen === "env" || screen === "host") {
    currentTab = screen;
  }
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.classList.toggle("active", tab.dataset.screen === screen);
  }
  const sectionId =
    screen === "env" || screen === "host" ? "screen-bottles" : `screen-${screen}`;
  for (const section of document.querySelectorAll<HTMLElement>(".screen")) {
    section.hidden = section.id !== sectionId;
  }
  if (screen === "doctor") {
    mountDoctorConsole();
    loadDoctor().catch((error: unknown) => {
      showToast(error instanceof Error ? error.message : String(error));
    });
  } else {
    destroyDoctorConsole();
  }
  if (screen === "env" || screen === "host") {
    renderAll(rowsCache);
  }
  // Вкладка — часть состояния окна (ui.tab), восстанавливается при запуске.
  try {
    persistTab(gui(), screen);
  } catch {
    // Мост ещё не привязан (монтирование до разблокировки) — не страшно.
  }
}

/** Диалог настроек (Task 18): смена мастер-пароля + папка данных. */
function openSettingsDialog(): void {
  showSettings({
    changePassword: (oldPassword, newPassword) =>
      gui().changePassword(oldPassword, newPassword),
    dataDir: () => gui().dataDir(),
    onInfo: (message) => showToast(message, "success"),
  });
}

/** Консоль Диагностики (Task 17): живёт, только пока вкладка открыта. */
let doctorConsole: ConsoleHandle | null = null;

function mountDoctorConsole(): void {
  if (doctorConsole !== null) {
    return;
  }
  const rootEl = document.querySelector<HTMLElement>("#screen-doctor");
  if (rootEl === null) {
    return;
  }
  doctorConsole = mountConsole(rootEl, {
    daemonFeed: () => [...daemonLogFeed],
    guiRecent: () => gui().guiLogRecent(),
    onDaemonEvent: (fn) => {
      setDaemonLogListener(fn);
      return () => setDaemonLogListener(null);
    },
    stats: () => gui().daemonStats(),
  });
}

function destroyDoctorConsole(): void {
  doctorConsole?.destroy();
  doctorConsole = null;
}

/** «Собрать логи (ZIP)»: путь — тостом; папка открывается соседней кнопкой. */
async function onCollectLogs(): Promise<void> {
  try {
    lastLogsPath = await collectLogs();
    showToast(`Логи собраны: ${lastLogsPath}`, "success");
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  }
}

/** Подтверждение разрушительных действий; подменяется в тестах. */
let confirmAction: (message: string) => boolean = (message) =>
  window.confirm(message);

export function setConfirmAction(fn: (message: string) => boolean): void {
  confirmAction = fn;
}

/**
 * «Обновить аплаенс» / «Сбросить аплаенс»: служебные команды демона.
 */
async function onApplianceUpgrade(force: boolean): Promise<void> {
  if (
    force &&
    !confirmAction(
      "Сброс аплаенса остановит все бутылки, перезапустит WSL " +
        "(все его дистрибутивы) и переустановит аплаенс с нуля. Продолжить?",
    )
  ) {
    return;
  }
  const args = force
    ? ["--json", "upgrade", "--force"]
    : ["--json", "upgrade"];
  try {
    const { runBottleReply } = await import("./ipc");
    const reply = await runBottleReply(args);
    if ("Upgraded" in reply) {
      const { previous, current } = reply.Upgraded;
      showToast(
        previous !== null
          ? `Аплаенс обновлён: версия ${previous} → ${current}`
          : `Аплаенс обновлён: версия ${current}`,
        "success",
      );
    } else if ("Error" in reply) {
      showToast(reply.Error.message);
    } else {
      showToast("Неожиданный ответ демона на upgrade");
    }
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  }
  await loadDoctor().catch(() => undefined);
}

/** Открыть редактор бутылки (окно ОС в Tauri, overlay в демо). */
function openEditorWindow(id: string | null, state: string | null): void {
  gui()
    .openEditor(id, state)
    .catch((error: unknown) => {
      showToast(error instanceof Error ? error.message : String(error));
    });
}

/**
 * Монтирует обработчики, делает первое подключение к демону и запускает
 * watchdog. `once: true` — только обработчики (тесты), без интервала.
 */
export function mountApp(options?: { once?: boolean }): { noop: boolean } {
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.addEventListener("click", () => {
      switchScreen(tab.dataset.screen ?? "env");
    });
  }
  document
    .querySelector<HTMLButtonElement>("#create-open")
    ?.addEventListener("click", () => {
      openEditorWindow(null, null);
    });
  document
    .querySelector<HTMLButtonElement>("#settings-open")
    ?.addEventListener("click", () => {
      openSettingsDialog();
    });
  // Горячие клавиши (спека §9): Ctrl+, настройки, Ctrl+L Диагностика,
  // F5 обновить список и статусы.
  installHotkeys({
    openSettings: openSettingsDialog,
    openDoctor: () => switchScreen("doctor"),
    refresh: () => {
      void refreshBottles();
      void daemonWatchdog();
    },
  });
  // Состояние окна из БД: размер (ui.win_w/ui.win_h) и последняя вкладка.
  void restoreWindowState(gui())
    .then((savedTab) => {
      if (
        savedTab === "env" ||
        savedTab === "host" ||
        savedTab === "doctor"
      ) {
        switchScreen(savedTab);
      }
    })
    .catch(() => undefined);
  // Мутации БД из окна редактора: тост + обновление сетки (Task 16).
  setBottlesChangedListener((payload) => {
    const messages = {
      created: `Бутылка ${payload.id} создана`,
      updated: `Профиль ${payload.id} обновлён`,
      deleted: `Бутылка ${payload.id} удалена`,
    } as const;
    showToast(messages[payload.action], "success");
    void refreshBottles();
  });
  document
    .querySelector<HTMLButtonElement>("#doctor-refresh")
    ?.addEventListener("click", () => {
      loadDoctor().catch((error: unknown) => {
        showToast(error instanceof Error ? error.message : String(error));
      });
    });
  document
    .querySelector<HTMLButtonElement>("#logs-export")
    ?.addEventListener("click", () => {
      void onCollectLogs();
    });
  document
    .querySelector<HTMLButtonElement>("#logs-open-dir")
    ?.addEventListener("click", () => {
      // «Открыть папку логов» — всегда доступна (спека §3.4).
      void gui()
        .logsDir()
        .then((dir) => {
          if (revealInDir !== null) {
            void revealInDir(dir);
          }
        })
        .catch((error: unknown) => {
          showToast(error instanceof Error ? error.message : String(error));
        });
    });
  document
    .querySelector<HTMLButtonElement>("#appliance-upgrade")
    ?.addEventListener("click", () => {
      void onApplianceUpgrade(false);
    });
  document
    .querySelector<HTMLButtonElement>("#appliance-reset")
    ?.addEventListener("click", () => {
      void onApplianceUpgrade(true);
    });
  document
    .querySelector<HTMLButtonElement>("#cmd-daemon-start")
    ?.addEventListener("click", () => {
      void onStartDaemon();
    });
  document
    .querySelector<HTMLButtonElement>("#cmd-daemon-stop")
    ?.addEventListener("click", () => {
      void onDaemonStop();
    });
  document
    .querySelector<HTMLButtonElement>("#cmd-console")
    ?.addEventListener("click", () => {
      switchScreen("doctor");
    });
  // Первичное наполнение: список из БД (демон не нужен), затем попытка
  // подключения к демону (упавший — не блокирует список).
  void refreshBottles().then(async () => {
    try {
      await gui().daemonConnect();
      daemonDown = false;
      remerge();
    } catch (error) {
      if (isDaemonDown(error)) {
        daemonDown = true;
        runtimeCache.clear();
        remerge();
      } else {
        showToast(error instanceof Error ? error.message : String(error));
      }
    }
  });
  if (options?.once !== true) {
    window.setInterval(() => {
      void daemonWatchdog();
    }, WATCHDOG_MS);
    // Размер окна — в БД с дебаунсом (восстановит restoreWindowState).
    installWindowSizePersist(gui());
  }
  return { noop: true };
}
