// Демо-мосты для разработки UI без демона и Tauri (`index.html?demo=1`
// в vite dev/build). Мост GuiApi (0.3): список из «БД», старт/стоп
// меняют in-memory состояние и присылают push-события, как живой демон.
// `?demo=down` — режим «демон не запущен» (кнопки погашены, панель
// предлагает запуск). В проде не используется: main.ts выбирает его
// только по query-параметру.

import {
  handleDaemonEvent,
  mountApp,
  setDaemonApi,
  setRevealInDir,
  switchScreen,
} from "./app";
import { dispatchBottlesChanged, setGuiApi } from "./backend";
import { mountEditor, setEditorHost } from "./editor";
import type {
  BottleEditorDataWire,
  BottleListRowWire,
  DaemonStatsWire,
  SaveBottleArgsWire,
} from "./ipc";
import { setBottleInvoker } from "./ipc";
import type { BottleOutput } from "./ipc";

const reply = (stdout: object, code = 0): BottleOutput => ({
  stdout: JSON.stringify(stdout),
  code,
});

/** Профили демо-«БД»: два env (WireGuard/OpenVPN) и host-клиент. */
const PROFILES: BottleListRowWire[] = [
  {
    id: "office-a",
    name: "Офис А (WireGuard)",
    kind: "env",
    protocol: "wireguard",
    has_credentials: false,
  },
  {
    id: "office-b",
    name: "Офис Б (OpenVPN)",
    kind: "env",
    protocol: "openvpn",
    has_credentials: true,
  },
  {
    id: "corp-test",
    name: "Тестовый контур",
    kind: "env",
    protocol: "openvpn",
    has_credentials: false,
  },
  {
    id: "legacy-ac",
    name: "Legacy Cisco AnyConnect",
    kind: "host",
    protocol: null,
    has_credentials: false,
  },
];

interface DemoRuntime {
  state: string;
  detail: string | null;
  socks5: number | null;
  forwards: { port: number; target: string }[];
}

/** Рантайм демо: office-a поднята, corp-test деградировала. */
const runtime = new Map<string, DemoRuntime>([
  [
    "office-a",
    {
      state: "up",
      detail: null,
      socks5: 55123,
      forwards: [{ port: 13389, target: "10.20.30.15:3389" }],
    },
  ],
  ["office-b", { state: "stopped", detail: null, socks5: null, forwards: [] }],
  [
    "corp-test",
    {
      state: "degraded",
      detail: "handshake не проходит 94 с",
      socks5: 55201,
      forwards: [],
    },
  ],
  ["legacy-ac", { state: "stopped", detail: null, socks5: null, forwards: [] }],
]);

/** Данные редактора для демо-бутылок (getBottle в overlay-режиме). */
const DEMO_EDITOR = new Map<string, BottleEditorDataWire>([
  [
    "office-a",
    {
      profile: {
        id: "office-a",
        name: "Офис А (WireGuard)",
        kind: "env",
        vpn: { protocol: "wireguard", config: "wg-office-a.conf" },
        expose: {
          socks5: 55123,
          forwards: [{ listen: 13389, target: "10.20.30.15:3389" }],
          dns: "10.20.30.1",
          launch: "mstsc /v:127.0.0.1:13389",
        },
      },
      has_credentials: false,
      has_config: true,
    },
  ],
  [
    "office-b",
    {
      profile: {
        id: "office-b",
        name: "Офис Б (OpenVPN)",
        kind: "env",
        vpn: { protocol: "openvpn", config: "office-b.ovpn" },
        expose: { socks5: 0, forwards: [], dns: null, launch: null },
      },
      has_credentials: true,
      has_config: true,
    },
  ],
  [
    "corp-test",
    {
      profile: {
        id: "corp-test",
        name: "Тестовый контур",
        kind: "env",
        vpn: { protocol: "openvpn", config: "corp-test.ovpn" },
        expose: { socks5: 0, forwards: [], dns: null, launch: null },
      },
      has_credentials: false,
      has_config: true,
    },
  ],
  [
    "legacy-ac",
    {
      profile: {
        id: "legacy-ac",
        name: "Legacy Cisco AnyConnect",
        kind: "host",
        vpn: {
          client: "C:\\vpn\\anyconnect.exe",
          endpoints: ["vpn.legacy.example.com"],
        },
        expose: { socks5: 0, forwards: [], dns: null, launch: null },
      },
      has_credentials: false,
      has_config: false,
    },
  ],
]);

/** Демон «жив» в демо (режим down переводит в false). */
let daemonAlive = true;

function pushState(id: string): void {
  const live = runtime.get(id);
  if (live === undefined) {
    return;
  }
  handleDaemonEvent({
    StateChanged: {
      id,
      state: live.state,
      detail: live.detail,
      socks5: live.socks5,
      forwards: live.forwards,
    },
  });
}

function pushLog(event: string, desc: string): void {
  handleDaemonEvent({
    Log: {
      ts: new Date().toISOString(),
      level: "info",
      src: "daemon",
      event,
      desc,
      ctx: {},
    },
  });
}

const STATS: DaemonStatsWire = {
  pid: 4242,
  version: "0.3.0-demo",
  started_at: new Date(Date.now() - 3_600_000).toISOString(),
  active_bottles: 2,
  total_profiles: PROFILES.length,
  mem_bytes: 48 * 1024 * 1024,
  cpu_pct: 1.7,
  wsl_state: "running",
  appliance_version: "0.3.0",
};

const DOCTOR = {
  DoctorReport: {
    items: [
      { code: "daemon.alive", status: "Ok", detail: "демон bottled отвечает (v0.3.0-demo)" },
      { code: "wsl.appliance", status: "Ok", detail: "апплайенс Погребка импортирован, rootfs актуален" },
      { code: "wsl.running", status: "Ok", detail: "виртуальная машина запущена" },
      { code: "agent.ping", status: "Ok", detail: "bottle-agent внутри VM отвечает за 12 мс" },
      { code: "ports.conflict", status: "Warn", detail: "порт 13389 занят двумя профилями — при старте обоих будет конфликт" },
      { code: "svc.routes", status: "Fail", detail: "служба маршрутов bottle-svc не установлена — host-бутылки недоступны" },
      { code: "store.profiles", status: "Ok", detail: "4 профиля в БД, битых записей нет" },
    ],
  },
};

/** Подключает демо-мосты и монтирует приложение (точка входа ?demo=1). */
export function mountDemo(down: boolean, view?: string | null): void {
  daemonAlive = !down;

  setGuiApi({
    listBottles: async () => PROFILES.map((row) => ({ ...row })),
    daemonConnect: async () => {
      if (!daemonAlive) {
        throw new Error("не удалось подключиться к демону — запущен ли bottled?");
      }
      // Как живой демон после SyncProfiles: полная картина состояний.
      for (const id of runtime.keys()) {
        pushState(id);
      }
      pushLog("profiles.synced", "профили синхронизированы: 4 шт");
    },
    startBottle: async (id) => {
      const live = runtime.get(id);
      if (live === undefined) {
        throw new Error(`бутылка не найдена: ${id}`);
      }
      live.state = "starting";
      live.detail = null;
      pushState(id);
      pushLog("bottle.starting", `бутылка ${id} поднимается`);
      window.setTimeout(() => {
        live.state = "up";
        live.socks5 = live.socks5 ?? 55000 + Math.floor(Math.random() * 900);
        pushState(id);
        pushLog("bottle.up", `бутылка ${id} работает`);
      }, 900);
    },
    stopBottle: async (id) => {
      const live = runtime.get(id);
      if (live === undefined) {
        throw new Error(`бутылка не найдена: ${id}`);
      }
      live.state = "stopping";
      pushState(id);
      window.setTimeout(() => {
        live.state = "stopped";
        live.detail = null;
        live.socks5 = null;
        live.forwards = [];
        pushState(id);
        pushLog("bottle.stopped", `бутылка ${id} остановлена`);
      }, 700);
    },
    stopDaemon: async () => {
      daemonAlive = false;
      for (const live of runtime.values()) {
        live.state = "stopped";
        live.socks5 = null;
        live.forwards = [];
      }
      pushLog("daemon.shutdown", "демон завершил работу по команде GUI");
    },
    daemonStats: async () => {
      if (!daemonAlive) {
        throw new Error("не удалось подключиться к демону — запущен ли bottled?");
      }
      return { ...STATS };
    },
    updateCredentials: async (id) => {
      const profile = PROFILES.find((row) => row.id === id);
      if (profile === undefined) {
        throw new Error(`бутылка не найдена: ${id}`);
      }
      profile.has_credentials = true;
      const data = DEMO_EDITOR.get(id);
      if (data !== undefined) {
        data.has_credentials = true;
      }
    },
    logEvent: async () => {},
    guiLogRecent: async () => [
      {
        ts: new Date(Date.now() - 50_000).toISOString(),
        level: "info",
        src: "gui",
        event: "db.unlocked",
        desc: "Хранилище разблокировано",
        ctx: {},
      },
      {
        ts: new Date(Date.now() - 40_000).toISOString(),
        level: "info",
        src: "gui",
        event: "bottle.start.requested",
        desc: "Пользователь запустил бутылку office-a",
        ctx: { id: "office-a" },
      },
      {
        ts: new Date(Date.now() - 30_000).toISOString(),
        level: "warn",
        src: "gui",
        event: "daemon.disconnected",
        desc: "Потеряно соединение с демоном, переподключение…",
        ctx: {},
      },
    ],
    logsDir: async () => "/tmp/pogrebok-logs-demo",
    // Редактор в демо — overlay поверх главного (нет Tauri): та же
    // форма editor.ts, но окно ОС заменяет затемнённая подложка.
    openEditor: async (id, state) => {
      const overlay = document.createElement("div");
      overlay.className = "editor-overlay";
      const panel = document.createElement("div");
      panel.className = "editor-panel";
      overlay.append(panel);
      document.body.append(overlay);
      setEditorHost({
        context: async () => ({ id, state }),
        pickConfig: async () => "C:\\vpn\\demo-config.conf",
        close: () => {
          overlay.remove();
        },
      });
      await mountEditor(panel);
    },
    getBottle: async (id) => DEMO_EDITOR.get(id) ?? null,
    saveBottle: async (args: SaveBottleArgsWire) => {
      const existing = PROFILES.find((row) => row.id === args.id);
      if (!args.is_edit && existing !== undefined) {
        throw new Error(
          `бутылка с id «${args.id}» уже существует — выберите другой id`,
        );
      }
      if (args.is_edit && existing === undefined) {
        throw new Error(`бутылка не найдена: ${args.id}`);
      }
      const hasCreds =
        (existing?.has_credentials ?? false) || args.save_credentials;
      const row: BottleListRowWire = {
        id: args.id,
        name: args.name,
        kind: args.kind,
        protocol: args.kind === "env" ? (args.protocol ?? "wireguard") : null,
        has_credentials: hasCreds,
      };
      if (existing === undefined) {
        PROFILES.push(row);
        runtime.set(args.id, {
          state: "stopped",
          detail: null,
          socks5: null,
          forwards: [],
        });
      } else {
        Object.assign(existing, row);
      }
      DEMO_EDITOR.set(args.id, {
        profile: {
          id: args.id,
          name: args.name,
          kind: args.kind,
          vpn:
            args.kind === "env"
              ? {
                  protocol: args.protocol ?? "wireguard",
                  config: args.config_path ?? `${args.id}.conf`,
                }
              : {
                  client: args.client ?? "",
                  args: args.args,
                  endpoints: args.endpoints ?? [],
                  interface: args.interface,
                },
          expose: {
            socks5: args.socks5,
            forwards: args.forwards.map((target) => ({ listen: 0, target })),
            dns: args.dns ?? null,
            launch: args.launch ?? null,
          },
        },
        has_credentials: hasCreds,
        has_config: args.kind === "env",
      });
      dispatchBottlesChanged({
        action: args.is_edit ? "updated" : "created",
        id: args.id,
      });
    },
    deleteBottle: async (id) => {
      const index = PROFILES.findIndex((row) => row.id === id);
      if (index === -1) {
        throw new Error(`бутылка не найдена: ${id}`);
      }
      PROFILES.splice(index, 1);
      DEMO_EDITOR.delete(id);
      runtime.delete(id);
      dispatchBottlesChanged({ action: "deleted", id });
    },
  });

  // CLI-мост нужен только экрану «Диагностика» (doctor/logs) — в 0.3
  // эти команды служебные и остаются на CLI до Task 19.
  setBottleInvoker(async (args) => {
    const [cmd] = args[0] === "--json" ? args.slice(1) : args;
    if (cmd === "doctor") {
      return reply(DOCTOR);
    }
    if (cmd === "logs") {
      return reply({ Path: "/tmp/pogrebok-logs-demo.zip" });
    }
    if (cmd === "upgrade") {
      return reply({ Upgraded: { previous: "0.2.0", current: "0.3.0-demo" } });
    }
    return reply({ Done: { warning: null } });
  });

  setDaemonApi({
    start: async () => {
      daemonAlive = true;
    },
    status: async () => ({
      running: daemonAlive,
      log_tail:
        "2026-10-22T16:02:14Z INFO  bottled: слушаю named pipe \\\\.\\pipe\\bottle\n" +
        "2026-10-22T16:02:15Z INFO  bottled: апплайенс Погребка найден, версия 0.3.0\n" +
        "2026-10-22T16:02:15Z INFO  bottled: GUI подключился, профили синхронизированы: 4",
    }),
  });
  setRevealInDir(async () => {});
  // tray в демо не подключён (нет Tauri); действия идут через окно.
  mountApp();
  // ?view=create|doctor — сразу открыть нужный экран (скриншоты, Manual QA).
  if (view === "create" || view === "doctor") {
    switchScreen(view);
  }
}
