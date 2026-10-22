// Мост GUI→Rust-команды 0.3 (единый API для app.ts и тестов):
// список бутылок из БД (работает без демона), старт/стоп и статистика
// через DaemonClient, события демона — push из event-цикла (Task 11).
//
// В проде привязка — Tauri invoke/listen (main-tauri.ts); тесты и
// демо-режим подставляют заглушки через setGuiApi.

import type {
  BottleListRowWire,
  DaemonEventWire,
  DaemonStatsWire,
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

/** Признак «демон не запущен» в ошибках команд 0.3. */
export function isDaemonDown(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return (
    message.includes("не удалось подключиться к демону") ||
    message.includes("нет соединения с демоном")
  );
}
