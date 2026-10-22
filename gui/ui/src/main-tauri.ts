// Точка входа GUI (M4) внутри Tauri: подключает мосты к Rust-командам
// (run_bottle, start_daemon/daemon_status, диалог выбора файла, opener
// папки логов), tray (меню бутылок — Task 5) и монтирует каркас
// приложения. Загружается из main.ts только когда нет ?demo-параметра,
// чтобы Tauri-модули не тянулись в обычный браузер.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  activeBottleIds,
  handleDaemonDisconnected,
  handleDaemonEvent,
  mountApp,
  performBottleAction,
  setDaemonApi,
  setRevealInDir,
  setRowsNotifier,
} from "./app";
import {
  dispatchBottlesChanged,
  dispatchDaemonDisconnected,
  dispatchDaemonEvent,
  setDaemonDisconnectListener,
  setDaemonEventListener,
  setGuiApi,
} from "./backend";
import { setBottleInvoker } from "./ipc";
import type {
  BottleOutput,
  BottlesChangedWire,
  DaemonEventWire,
} from "./ipc";
import { renderLock } from "./lock";
import type { ImportOutcome, LockStatus } from "./lock";
import { makeTauriGuiApi } from "./tauri-api";
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

setRevealInDir(async (path) => {
  await revealItemInDir(path);
});

// Мост GUI→Rust-команды 0.3 (Task 15/16): список из БД, управление
// демоном и бутылками, журнал, редактор. Общая фабрика с окном редактора.
setGuiApi(makeTauriGuiApi());

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

// Мутации БД из окна редактора (Task 16): save/delete шлют
// "bottles-changed" всем окнам — главное обновляет сетку.
await listen<BottlesChangedWire>("bottles-changed", (event) => {
  dispatchBottlesChanged(event.payload);
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
