// Точка входа GUI (M4): подключает мосты к Rust-командам (run_bottle,
// start_daemon/daemon_status) и монтирует каркас приложения.

import { invoke } from "@tauri-apps/api/core";
import { mountApp, setDaemonApi } from "./app";
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

mountApp();
