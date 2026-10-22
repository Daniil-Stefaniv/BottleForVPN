// Точка входа GUI (M4) внутри Tauri: подключает мосты к Rust-командам
// (run_bottle, start_daemon/daemon_status, диалог выбора файла, opener
// папки логов), tray (меню бутылок — Task 5) и монтирует каркас
// приложения. Загружается из main.ts только когда нет ?demo-параметра,
// чтобы Tauri-модули не тянулись в обычный браузер.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  activeBottleIds,
  handleDaemonDisconnected,
  handleDaemonEvent,
  mountApp,
  performBottleAction,
  setConfigFileDialog,
  setDaemonApi,
  setRevealInDir,
  setRowsNotifier,
} from "./app";
import {
  dispatchDaemonDisconnected,
  dispatchDaemonEvent,
  setDaemonDisconnectListener,
  setDaemonEventListener,
  setGuiApi,
} from "./backend";
import { setBottleInvoker } from "./ipc";
import type {
  BottleListRowWire,
  BottleOutput,
  DaemonEventWire,
  DaemonStatsWire,
} from "./ipc";
import { renderLock } from "./lock";
import type { ImportOutcome, LockStatus } from "./lock";
import { initTray } from "./tray";

setBottleInvoker(async (args) => {
  return await invoke<BottleOutput>("run_bottle", { args });
});

setDaemonApi({
  start: async () => {
    await invoke("start_daemon");
  },
  status: async () => {
    return await invoke<{ running: boolean; log_tail: string }>(
      "daemon_status",
    );
  },
});

setConfigFileDialog(async () => {
  const selected = await open({
    multiple: false,
    directory: false,
    title: "Файл конфигурации туннеля",
  });
  return typeof selected === "string" ? selected : null;
});

setRevealInDir(async (path) => {
  await revealItemInDir(path);
});

// Мост GUI→Rust-команды 0.3 (Task 15): список из БД, управление демоном
// и бутылками, журнал. Имена аргументов — camelCase, Tauri разложит их
// в snake_case параметры команд.
setGuiApi({
  listBottles: () => invoke<BottleListRowWire[]>("list_bottles"),
  daemonConnect: () => invoke<void>("daemon_connect"),
  startBottle: (id) => invoke<void>("start_bottle", { id }),
  stopBottle: (id) => invoke<void>("stop_bottle", { id }),
  stopDaemon: () => invoke<void>("stop_daemon"),
  daemonStats: () => invoke<DaemonStatsWire>("daemon_stats"),
  updateCredentials: (id, username, password) =>
    invoke<void>("update_credentials", { id, username, password }),
  logEvent: (code, ctx) => invoke<void>("log_event", { code, ctx }),
});

// Push-события демона (Task 11): event-цикл Rust-стороны эмитит
// "daemon-event" (StateChanged/Log) и "daemon-disconnected" (разрыв).
setDaemonEventListener(handleDaemonEvent);
setDaemonDisconnectListener(handleDaemonDisconnected);
await listen<DaemonEventWire>("daemon-event", (event) => {
  dispatchDaemonEvent(event.payload);
});
await listen("daemon-disconnected", () => {
  dispatchDaemonDisconnected();
});

// Tray (Task 5): меню обновляется после каждого refresh списка,
// действия меню идут в тот же performBottleAction. Регистрируется до
// mountApp, чтобы первый refresh уже попал в меню.
const trayEnabled = await invoke<boolean>("tray_enabled");
if (trayEnabled) {
  setRowsNotifier(initTray((id, action) => {
    void performBottleAction(id, action);
  }));
  // Закрытие окна сворачивает в tray (выход — через меню «Завершить»).
  const window = getCurrentWindow();
  await window.onCloseRequested(async (event) => {
    event.preventDefault();
    await window.hide();
  });

  // «Завершить» в трее (0.3): подтверждение да/нет; «да» — Shutdown
  // демону и выход GUI (демон и GUI умирают вместе).
  let quitInProgress = false;
  await listen("quit-requested", async () => {
    if (quitInProgress) return;
    quitInProgress = true;
    try {
      const text = await invoke<string>("quit_confirm", {
        active: activeBottleIds(),
      });
      const yes = await ask(text, {
        title: "Погребок",
        kind: "warning",
        okLabel: "Завершить",
        cancelLabel: "Отмена",
      });
      if (yes) {
        await invoke("confirm_quit");
      }
    } finally {
      quitInProgress = false;
    }
  });
}

// Экран блокировки (0.3, пункт 7): без мастер-пароля GUI не пускает
// дальше — приложение монтируется только после разблокировки хранилища.
const lockStatus = await invoke<LockStatus>("vault_status");
const lockRoot = document.getElementById("lock-root")!;
renderLock(lockRoot, {
  status: lockStatus,
  api: {
    createVault: (password) => invoke("create_vault", { password }),
    unlockVault: (password) =>
      invoke<string | null>("unlock_vault", { password }),
    resetVault: () => invoke("reset_vault"),
    legacyImport: () => invoke<ImportOutcome>("legacy_import"),
  },
  onUnlocked: () => mountApp(),
});
