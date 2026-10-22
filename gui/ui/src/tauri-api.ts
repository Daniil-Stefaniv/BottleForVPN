// Фабрика привязок GuiApi → Tauri invoke (Task 16): общая для главного
// окна (main-tauri.ts) и окна редактора (editor-main.ts) — оба webview
// одного приложения, команды и состояние (Vault, журнал) общие.

import { invoke } from "@tauri-apps/api/core";
import type { GuiApi } from "./backend";
import type {
  BottleEditorDataWire,
  BottleListRowWire,
  DaemonStatsWire,
  SaveBottleArgsWire,
} from "./ipc";

/** GuiApi поверх Tauri-команд (camelCase аргументы → snake_case параметры). */
export function makeTauriGuiApi(): GuiApi {
  return {
    listBottles: () => invoke<BottleListRowWire[]>("list_bottles"),
    daemonConnect: () => invoke<void>("daemon_connect"),
    startBottle: (id) => invoke<void>("start_bottle", { id }),
    stopBottle: (id) => invoke<void>("stop_bottle", { id }),
    stopDaemon: () => invoke<void>("stop_daemon"),
    daemonStats: () => invoke<DaemonStatsWire>("daemon_stats"),
    updateCredentials: (id, username, password) =>
      invoke<void>("update_credentials", { id, username, password }),
    logEvent: (code, ctx) => invoke<void>("log_event", { code, ctx }),
    openEditor: (id, state) =>
      invoke<void>("open_editor", { id, bottleState: state }),
    getBottle: (id) => invoke<BottleEditorDataWire | null>("get_bottle", { id }),
    saveBottle: (args: SaveBottleArgsWire) =>
      invoke<void>("save_bottle", { args }),
    deleteBottle: (id) => invoke<void>("delete_bottle", { id }),
  };
}
