// Каркас приложения (M4 Task 3–4): поллинг списка каждые 2 с, кнопки
// старт/стоп/изменить, состояние «демон не запущен» с кнопкой запуска
// bottled (рулинг D1), тосты ошибок CLI дословно, переключение экранов
// список/создать/диагностика.

import { isDaemonUnavailable, runBottleReply, setBottleInvoker } from "./ipc";
import type { CliReply } from "./ipc";
import {
  mergeListStatus,
  renderListTable,
  type BottleAction,
  type BottleRow,
} from "./list";
import { loadProfileForEdit, resetForm, submitCreate } from "./create";
import { collectLogs, loadDoctor } from "./doctor";

/** API демона (Rust-команды start_daemon/daemon_status), подменяется в тестах. */
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

/** Интервал статус-поллинга (D1). */
const POLL_INTERVAL_MS = 2000;
/** Зонд при выключенном демоне: без него каждый тик спавнит CLI,
 * который 60 с ретраит connect (~30 висящих процессов, I4 ревью). */
const POLL_INTERVAL_DAEMON_DOWN_MS = 10000;

/** Задержка до следующего refresh: живой демон — 2 с, недоступный —
 * медленный зонд 10 с (демон мог быть запущен и вручную). */
export function nextPollDelayMs(daemonDown: boolean): number {
  return daemonDown ? POLL_INTERVAL_DAEMON_DOWN_MS : POLL_INTERVAL_MS;
}

/** Показывает ошибку тостом (дословное сообщение CLI/демона). */
function showToast(message: string): void {
  const toast = document.querySelector<HTMLElement>("#toast");
  if (toast === null) {
    return;
  }
  toast.textContent = message;
  toast.hidden = false;
  window.setTimeout(() => {
    toast.hidden = true;
  }, 8000);
}

/** Панель «демон не запущен»: показать с инструкцией / скрыть. */
function setDaemonPanel(visible: boolean): void {
  const panel = document.querySelector<HTMLElement>("#daemon-panel");
  if (panel === null) {
    return;
  }
  panel.hidden = !visible;
  if (visible) {
    const hint = panel.querySelector<HTMLElement>("#daemon-hint");
    if (hint !== null) {
      hint.textContent =
        "Демон BottleForApps не запущен. Нажмите «Запустить демона» " +
        "(bottled) или стартуйте его вручную — затем список обновится сам.";
    }
    void updateDaemonLog();
  }
}

/** Подтягивает хвост журнала демона в панель. */
async function updateDaemonLog(): Promise<void> {
  const log = document.querySelector<HTMLElement>("#daemon-log");
  if (log === null || daemonApi === null) {
    return;
  }
  try {
    const status = await daemonApi.status();
    log.textContent = status.log_tail || "(журнал пуст)";
  } catch {
    log.textContent = "";
  }
}

/** Подписчик на обновления строк таблицы (tray-меню — Task 5). */
let rowsNotifier: ((rows: BottleRow[]) => void) | null = null;

export function setRowsNotifier(fn: ((rows: BottleRow[]) => void) | null): void {
  rowsNotifier = fn;
}

/**
 * Однократное обновление таблицы: `--json list` + `--json status` →
 * merge → рендер. Ошибка «демон не запущен» — панель с инструкцией;
 * прочие ошибки — тост. Возвращает строки (пусто при любой ошибке).
 */
let daemonDown = false;

export async function refreshBottles(): Promise<BottleRow[]> {
  let list: CliReply;
  let status: CliReply | null = null;
  try {
    list = await runBottleReply(["--json", "list"]);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    daemonDown = isDaemonUnavailable(error);
    if (isDaemonUnavailable(error)) {
      setDaemonPanel(true);
    } else {
      setDaemonPanel(false);
      showToast(message);
    }
    renderListTable([], () => {});
    return [];
  }
  daemonDown = false;
  // Status — вспомогательный: его отказ (например, dev-демон без
  // менеджера на macOS) не прячет список — строки без рантайма (M7).
  try {
    status = await runBottleReply(["--json", "status"]);
  } catch {
    status = null;
  }
  daemonDown = false;
  setDaemonPanel(false);
  if (!("List" in list)) {
    showToast("Неожиданный ответ демона на list");
    return [];
  }
  const runtime = status !== null && "Status" in status ? status.Status.bottles : [];
  const rows = mergeListStatus(list.List.bottles, runtime);
  renderListTable(
    rows,
    (id, action) => {
      void performBottleAction(id, action);
    },
    (id) => {
      switchScreen("create");
      void loadProfileForEdit(id).then((loaded) => {
        if (!loaded) {
          showToast(`Профиль бутылки не найден: ${id}`);
        }
      });
    },
  );
  rowsNotifier?.(rows);
  return rows;
}

/**
 * Старт/стоп бутылки: `--json start|stop <id>`; warning демона и ошибки —
 * тостом дословно; после успеха список обновляется немедленно.
 */
export async function performBottleAction(
  id: string,
  action: BottleAction,
): Promise<void> {
  let reply: CliReply;
  try {
    reply = await runBottleReply(["--json", action, id]);
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
    return;
  }
  if ("Stopped" in reply && reply.Stopped.warning !== null) {
    showToast(reply.Stopped.warning);
  }
  await refreshBottles();
}

/** Кнопка «Запустить демона»: стартует bottled и обновляет список. */
async function onStartDaemon(): Promise<void> {
  if (daemonApi === null) {
    return;
  }
  try {
    await daemonApi.start();
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
    return;
  }
  // Демон поднимает каналы не мгновенно — поллинг доведёт список сам.
  await refreshBottles();
}

/** Переключение экранов: активный таб + видимость секций. */
export function switchScreen(screen: string): void {
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.classList.toggle("active", tab.dataset.screen === screen);
  }
  for (const section of document.querySelectorAll<HTMLElement>(".screen")) {
    section.hidden = section.id !== `screen-${screen}`;
  }
  if (screen === "doctor") {
    loadDoctor().catch((error: unknown) => {
      showToast(error instanceof Error ? error.message : String(error));
    });
  }
}

/** Submit формы create: edit определяется по readonly id (заполнен из show). */
async function onCreateSubmit(): Promise<void> {
  const idField = document.querySelector<HTMLInputElement>("#f-id");
  const isEdit = idField !== null && idField.readOnly;
  const ok = await submitCreate(isEdit);
  if (ok) {
    switchScreen("list");
    await refreshBottles();
  }
}

/** «Собрать логи»: путь + кнопка «Открыть папку» (Tauri opener). */
async function onCollectLogs(): Promise<void> {
  try {
    lastLogsPath = await collectLogs();
    const openButton = document.querySelector<HTMLElement>("#logs-open-dir");
    if (openButton !== null) {
      openButton.hidden = false;
    }
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error));
  }
}

/** Показ/скрытие полей по типу бутылки (env ↔ host). */
function syncKindFields(): void {
  const kind = document.querySelector<HTMLSelectElement>("#f-kind")?.value;
  for (const field of document.querySelectorAll<HTMLElement>(".env-field")) {
    field.hidden = kind !== "env";
  }
  for (const field of document.querySelectorAll<HTMLElement>(".host-field")) {
    field.hidden = kind !== "host";
  }
}

/**
 * Монтирует обработчики и запускает поллинг. `once: true` — подключить
 * только обработчики (тесты), без интервала.
 */
export function mountApp(options?: { once?: boolean }): { noop: boolean } {
  const daemonStart = document.querySelector<HTMLButtonElement>("#daemon-start");
  if (daemonStart !== null) {
    daemonStart.addEventListener("click", () => {
      void onStartDaemon();
    });
  }
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.addEventListener("click", () => {
      switchScreen(tab.dataset.screen ?? "list");
    });
  }
  const form = document.querySelector<HTMLFormElement>("#create-form");
  form?.addEventListener("submit", (event) => {
    event.preventDefault();
    void onCreateSubmit();
  });
  document
    .querySelector<HTMLButtonElement>("#create-reset")
    ?.addEventListener("click", () => {
      resetForm();
      syncKindFields();
    });
  document
    .querySelector<HTMLSelectElement>("#f-kind")
    ?.addEventListener("change", syncKindFields);
  document
    .querySelector<HTMLButtonElement>("#f-config-pick")
    ?.addEventListener("click", () => {
      void pickConfigFile();
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
      if (lastLogsPath !== null && revealInDir !== null) {
        void revealInDir(lastLogsPath);
      }
    });
  syncKindFields();
  void refreshBottles();
  if (options?.once !== true) {
    // Поллинг с backoff: при недоступном демоне — медленный зонд
    // (I4), чтобы не плодить висящие CLI-процессы с 60-с ретраем.
    const scheduleNext = (): void => {
      window.setTimeout(() => {
        void refreshBottles().finally(scheduleNext);
      }, nextPollDelayMs(daemonDown));
    };
    scheduleNext();
    // Журнал демона в панели — тем же темпом.
    window.setInterval(() => {
      void updateDaemonLog();
    }, POLL_INTERVAL_MS);
  }
  return { noop: true };
}

/** Диалог выбора файла конфига (Tauri plugin-dialog; адаптер — в main.ts). */
let openFileDialog: (() => Promise<string | null>) | null = null;

export function setConfigFileDialog(fn: () => Promise<string | null>): void {
  openFileDialog = fn;
}

async function pickConfigFile(): Promise<void> {
  if (openFileDialog === null) {
    return;
  }
  const path = await openFileDialog();
  if (path !== null) {
    const configField = document.querySelector<HTMLInputElement>("#f-config");
    if (configField !== null) {
      configField.value = path;
    }
  }
}

// Реэкспорт для main.ts: единая точка подключения мостов.
export { setBottleInvoker };
