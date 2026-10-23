// Мост GUI→Rust-команды 0.3 (единый API для app.ts и тестов):
// список бутылок из БД (работает без демона), старт/стоп и статистика
// через DaemonClient, события демона — push из event-цикла (Task 11).
//
// В проде привязка — Tauri invoke/listen (main-tauri.ts); тесты и
// демо-режим подставляют заглушки через setGuiApi.

import type {
  BottleEditorDataWire,
  BottleListRowWire,
  BottlesChangedWire,
  DaemonEventWire,
  DaemonStatsWire,
  EventWire,
  SaveBottleArgsWire,
} from "./ipc";

/** API Rust-стороны GUI (команды из main.rs). */
export interface GuiApi {
  /** Бутылки из зашифрованной БД (демон не нужен). */
  listBottles(): Promise<BottleListRowWire[]>;
  /** Hello → SyncProfiles → запуск event-цикла (один раз за сессию). */
  daemonConnect(): Promise<void>;
  startBottle(id: string): Promise<void>;
  stopBottle(id: string): Promise<void>;
  /** Shutdown демону: бутылки останавливаются, демон завершается. */
  stopDaemon(): Promise<void>;
  /** Статистика демона; throw — демон недоступен. */
  daemonStats(): Promise<DaemonStatsWire>;
  /** Сохранить логин/пароль бутылки в БД и синхронизировать демону. */
  updateCredentials(id: string, username: string, password: string): Promise<void>;
  /** Запись в журнал GUI (коды — из реестра core::events). */
  logEvent(code: string, ctx: Record<string, unknown>): Promise<void>;
  /** Последние события журнала GUI (консоль Диагностики, Task 17). */
  guiLogRecent(): Promise<EventWire[]>;
  /** Папка журналов («Открыть папку логов»). */
  logsDir(): Promise<string>;
  /** Смена мастер-пароля (Task 18): null — успех, "wrong_password" — неверный старый. */
  changePassword(oldPassword: string, newPassword: string): Promise<string | null>;
  /** Папка данных приложения (диалог настроек). */
  dataDir(): Promise<string>;
  /** Настройка из meta БД (ui.*); отсутствующая — null. */
  getSetting(key: string): Promise<string | null>;
  /** Запись настройки в meta БД. */
  setSetting(key: string, value: string): Promise<void>;
  /** Восстановить размер главного окна из настроек. */
  applyWindowSize(width: number, height: number): Promise<void>;
  /** Открыть редактор бутылки (Task 16): окно ОС в Tauri, overlay в демо. */
  openEditor(id: string | null, state: string | null): Promise<void>;
  /** Профиль для формы редактирования; `null` — нет такой. */
  getBottle(id: string): Promise<BottleEditorDataWire | null>;
  /** «Создать»/«Сохранить» редактора: БД + синхронизация демону. */
  saveBottle(args: SaveBottleArgsWire): Promise<void>;
  /** Удаление из окна редактирования (БД + RemoveProfile демону). */
  deleteBottle(id: string): Promise<void>;
}

let api: GuiApi | null = null;

export function setGuiApi(next: GuiApi): void {
  api = next;
}

/** Текущий мост; без привязки — понятная ошибка (тест обязан stub'ить). */
export function gui(): GuiApi {
  if (api === null) {
    throw new Error("мост GUI не инициализирован (setGuiApi)");
  }
  return api;
}

/** Подписчик на push-события демона (event-цикл Rust-стороны). */
let daemonEventListener: ((event: DaemonEventWire) => void) | null = null;

export function setDaemonEventListener(
  fn: ((event: DaemonEventWire) => void) | null,
): void {
  daemonEventListener = fn;
}

/** Точка входа событий из main-tauri.ts (listen("daemon-event")). */
export function dispatchDaemonEvent(event: DaemonEventWire): void {
  daemonEventListener?.(event);
}

/** Подписчик на разрыв event-соединения (событие "daemon-disconnected"). */
let disconnectListener: (() => void) | null = null;

export function setDaemonDisconnectListener(fn: (() => void) | null): void {
  disconnectListener = fn;
}

export function dispatchDaemonDisconnected(): void {
  disconnectListener?.();
}

/** Подписчик на событие `bottles-changed` (мутация БД из окна редактора). */
let bottlesChangedListener: ((payload: BottlesChangedWire) => void) | null =
  null;

export function setBottlesChangedListener(
  fn: ((payload: BottlesChangedWire) => void) | null,
): void {
  bottlesChangedListener = fn;
}

/** Точка входа события из main-tauri.ts (listen("bottles-changed"))
 * и из демо-редактора (overlay сохраняет локально). */
export function dispatchBottlesChanged(payload: BottlesChangedWire): void {
  bottlesChangedListener?.(payload);
}

/** Признак «демон не запущен» в ошибках команд 0.3. */
export function isDaemonDown(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return (
    message.includes("не удалось подключиться к демону") ||
    message.includes("нет соединения с демоном")
  );
}
