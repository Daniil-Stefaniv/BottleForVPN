// Точка входа GUI (M4): подключает мосты к Rust-командам (run_bottle,
// start_daemon/daemon_status, диалог выбора файла, opener папки логов)
// и монтирует каркас приложения.

import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { mountApp, setDaemonApi, setRevealInDir, setConfigFileDialog } from "./app";
import { setBottleInvoker } from "./ipc";
import type { BottleOutput } from "./ipc";

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

mountApp();
