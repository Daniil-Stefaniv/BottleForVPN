// Каркас приложения (M4 Task 3): поллинг списка каждые 2 с, кнопки
// старт/стоп (логика — в list.ts), состояние «демон не запущен» с
// кнопкой запуска bottled (рулинг D1), тосты ошибок CLI дословно.

import {
  isDaemonUnavailable,
  runBottleReply,
  setBottleInvoker,
} from "./ipc";
import type { CliReply } from "./ipc";
import {
  mergeListStatus,
  renderListTable,
  type BottleAction,
  type BottleRow,
} from "./list";

/** API демона (Rust-команды start_daemon/daemon_status), подменяется в тестах. */
export interface DaemonApi {
  start: () => Promise<void>;
  status: () => Promise<{ running: boolean; log_tail: string }>;
}

let daemonApi: DaemonApi | null = null;

export function setDaemonApi(api: DaemonApi): void {
  daemonApi = api;
}

/** Интервал статус-поллинга (D1). */
const POLL_INTERVAL_MS = 2000;

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

/**
 * Однократное обновление таблицы: `--json list` + `--json status` →
 * merge → рендер. Ошибка «демон не запущен» — панель с инструкцией;
 * прочие ошибки — тост. Возвращает строки (пусто при любой ошибке).
 */
export async function refreshBottles(): Promise<BottleRow[]> {
  let list: CliReply;
  let status: CliReply;
  try {
    list = await runBottleReply(["--json", "list"]);
    status = await runBottleReply(["--json", "status"]);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (isDaemonUnavailable(error)) {
      setDaemonPanel(true);
    } else {
      setDaemonPanel(false);
      showToast(message);
    }
    renderListTable([], () => {});
    return [];
  }
  setDaemonPanel(false);
  if (!("List" in list) || !("Status" in status)) {
    showToast("Неожиданный ответ демона на list/status");
    return [];
  }
  const rows = mergeListStatus(list.List.bottles, status.Status.bottles);
  renderListTable(rows, (id, action) => {
    void performBottleAction(id, action);
  });
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
  // Демон поднимает каналы не мгновенно — первая попытка может уйти в
  // retry-окно CLI; поллинг доведёт список сам.
  await refreshBottles();
}

/**
 * Монтирует обработчики и запускает поллинг. `once: true` — подключить
 * только обработчики (тесты), без интервала.
 */
export function mountApp(options?: { once?: boolean }): { noop: boolean } {
  const startButton = document.querySelector<HTMLButtonElement>("#daemon-start");
  if (startButton !== null) {
    startButton.addEventListener("click", () => {
      void onStartDaemon();
    });
  }
  void refreshBottles();
  if (options?.once !== true) {
    window.setInterval(() => {
      void refreshBottles();
    }, POLL_INTERVAL_MS);
    // Журнал демона в панели — тем же темпом.
    window.setInterval(() => {
      void updateDaemonLog();
    }, POLL_INTERVAL_MS);
  }
  return { noop: true };
}

// Реэкспорт для main.ts: единая точка подключения мостов.
export { setBottleInvoker };
