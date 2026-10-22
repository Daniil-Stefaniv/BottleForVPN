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
  mountApp,
  performBottleAction,
  setConfigFileDialog,
  setDaemonApi,
  setRevealInDir,
  setRowsNotifier,
} from "./app";
import { setBottleInvoker } from "./ipc";
import type { BottleOutput } from "./ipc";
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

mountApp();
