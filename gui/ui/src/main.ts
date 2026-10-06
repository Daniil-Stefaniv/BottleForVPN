// Точка входа GUI (M4): подключает мосты к Rust-командам (run_bottle,
// start_daemon/daemon_status, диалог выбора файла, opener папки логов),
// tray (меню бутылок — Task 5) и монтирует каркас приложения.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
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
  // Закрытие окна сворачивает в tray (выход — через меню «Выход»).
  const window = getCurrentWindow();
  await window.onCloseRequested(async (event) => {
    event.preventDefault();
    await window.hide();
  });
}

mountApp();
