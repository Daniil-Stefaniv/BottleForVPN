// Tray-мост (M4 Task 5): меню «Показать окно» · бутылки (старт/стоп,
// макс 8 + «…» — режет Rust) · «Выход». Rust шлёт ивент `tray-action`,
// здесь он маршрутизируется; подписи пунктов готовятся из строк таблицы.
// Show обрабатывается в Rust (окно показывается без прав у TS).

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { BottleRow } from "./list";
import { stateLabel } from "./list";
import type { BottleAction } from "./list";

/** Запись для tray-меню (контракт Rust-команды update_tray). */
export interface TrayEntry {
  id: string;
  label: string;
  running: boolean;
}

/** Формирует подписи: «id — имя (состояние)»; running = up. */
export function buildTrayEntries(rows: BottleRow[]): TrayEntry[] {
  return rows.map((row) => ({
    id: row.id,
    label: `${row.id} — ${row.name === "" ? "(без имени)" : row.name} (${stateLabel(row.state, row.reason)})`,
    running: row.state === "up",
  }));
}

/** Ивент от Rust-меню (serde tag=kind, snake_case). */
export type TrayEvent =
  | { kind: "show" }
  | { kind: "start"; id: string }
  | { kind: "stop"; id: string }
  | { kind: "more" }
  | { kind: "exit" };

/** Маршрутизация ивента меню: старт/стоп — в действие бутылки. */
export function handleTrayEvent(
  event: TrayEvent,
  handlers: { onBottleAction: (id: string, action: BottleAction) => void },
): void {
  if (event.kind === "start" || event.kind === "stop") {
    handlers.onBottleAction(event.id, event.kind);
  }
  // show — Rust показывает окно сам; more/exit — обрабатывает Rust/поллинг.
}

/**
 * Подключает tray: обновление меню после каждого refresh списка и
 * подписка на действия меню. Возвращает notify(rows) для app.ts.
 */
export function attachTray(deps: {
  onBottleAction: (id: string, action: BottleAction) => void;
  update: (entries: TrayEntry[]) => Promise<void>;
  subscribe: (handler: (event: TrayEvent) => void) => Promise<() => void>;
}): (rows: BottleRow[]) => void {
  const notify = (rows: BottleRow[]): void => {
    void deps.update(buildTrayEntries(rows));
  };
  void deps.subscribe((event) => {
    handleTrayEvent(event, { onBottleAction: deps.onBottleAction });
  });
  return notify;
}

/** Продакшн-подключение (main.ts): invoke + listen. */
export function initTray(
  onBottleAction: (id: string, action: BottleAction) => void,
): (rows: BottleRow[]) => void {
  return attachTray({
    onBottleAction,
    update: async (entries) => {
      await invoke("update_tray", { entries });
    },
    subscribe: async (handler) => {
      const unlisten = await listen<TrayEvent>("tray-action", (event) => {
        handler(event.payload);
      });
      return unlisten;
    },
  });
}
