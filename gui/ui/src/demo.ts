// Демо-мосты для разработки UI без демона и Tauri (`index.html?demo=1`
// в vite dev/build). Данные — правдоподобный срез четырёх бутылок;
// `?demo=down` — режим «демон не запущен» (панель с кнопкой запуска).
// В проде не используется: main.ts выбирает его только по query-параметру.

import { mountApp, setDaemonApi, setRevealInDir, switchScreen } from "./app";
import { setBottleInvoker } from "./ipc";
import type { BottleOutput } from "./ipc";

const reply = (stdout: object, code = 0): BottleOutput => ({
  stdout: JSON.stringify(stdout),
  code,
});

const LIST = {
  List: {
    bottles: [
      { id: "office-a", name: "Офис А (WireGuard)", kind: "env", state: "Up" },
      { id: "office-b", name: "Офис Б (OpenVPN)", kind: "env", state: "Stopped" },
      { id: "corp-test", name: "Тестовый контур", kind: "env", state: null },
      { id: "legacy-ac", name: "Legacy Cisco AnyConnect", kind: "host", state: "Stopped" },
    ],
  },
};

const STATUS = {
  Status: {
    bottles: [
      {
        id: "office-a",
        state: "Up",
        socks5: 55123,
        forwards: [{ port: 13389, target: "10.20.30.15:3389" }],
      },
      { id: "office-b", state: "Stopped", socks5: null, forwards: [] },
      {
        id: "corp-test",
        state: { Degraded: { reason: "handshake не проходит 94 с" } },
        socks5: 55201,
        forwards: [],
      },
      { id: "legacy-ac", state: "Stopped", socks5: null, forwards: [] },
    ],
  },
};

const DOCTOR = {
  DoctorReport: {
    items: [
      { code: "daemon.alive", status: "Ok", detail: "демон bottled отвечает (v0.2.0)" },
      { code: "wsl.appliance", status: "Ok", detail: "апплайенс BottleForApps импортирован, rootfs актуален" },
      { code: "wsl.running", status: "Ok", detail: "виртуальная машина запущена" },
      { code: "agent.ping", status: "Ok", detail: "bottle-agent внутри VM отвечает за 12 мс" },
      { code: "ports.conflict", status: "Warn", detail: "порт 13389 занят двумя профилями — при старте обоих будет конфликт" },
      { code: "svc.routes", status: "Fail", detail: "служба маршрутов bottle-svc не установлена — host-бутылки недоступны" },
      { code: "store.profiles", status: "Ok", detail: "4 профиля, битых записей нет" },
    ],
  },
};

/** Состояние демо: up-статус office-a, чтобы старт/стоп меняли экран. */
let officeAUp = true;

function demoAnswer(args: string[]): BottleOutput {
  // app.ts вызывает CLI как `bottle --json <cmd> ...` — флаг пропускаем.
  const [cmd, id] = args[0] === "--json" ? args.slice(1) : args;
  if (cmd === "list") {
    return reply(LIST);
  }
  if (cmd === "status") {
    if (!officeAUp) {
      return reply({
        Status: {
          bottles: STATUS.Status.bottles.map((b) =>
            b.id === "office-a" ? { ...b, state: "Stopped", socks5: null, forwards: [] } : b,
          ),
        },
      });
    }
    return reply(STATUS);
  }
  if (cmd === "show") {
    return reply({
      Show: {
        profile: {
          id: id ?? "office-a",
          name: "Офис А (WireGuard)",
          kind: "env",
          vpn: { protocol: "wireguard", config: "wg-office-a.conf" },
          expose: {
            socks5: 55123,
            forwards: [{ listen: 13389, target: "10.20.30.15:3389" }],
            dns: "10.20.30.1",
            launch: "mstsc",
          },
        },
        runtime: null,
      },
    });
  }
  if (cmd === "doctor") {
    return reply(DOCTOR);
  }
  if (cmd === "logs") {
    return reply({ Path: "/tmp/bottleforapps-logs-demo.zip" });
  }
  if (cmd === "start") {
    if (id === "office-a") {
      officeAUp = true;
    }
    return reply({ Started: { id: id ?? "?" } });
  }
  if (cmd === "stop") {
    if (id === "office-a") {
      officeAUp = false;
    }
    return reply({ Stopped: { id: id ?? "?", warning: null } });
  }
  if (cmd === "delete") {
    return reply({ Deleted: { id: id ?? "?", warning: null } });
  }
  if (cmd === "create") {
    return reply({ Created: { id: "demo" } });
  }
  return reply({ Done: { warning: null } });
}

/** Подключает демо-мосты и монтирует приложение (точка входа ?demo=1). */
export function mountDemo(down: boolean, view?: string | null): void {
  setBottleInvoker(async (args) => {
    if (down) {
      throw new Error(
        "Не удалось подключиться к демону BottleForApps — запущен ли bottled?",
      );
    }
    return demoAnswer(args);
  });
  setDaemonApi({
    start: async () => {},
    status: async () => ({
      running: false,
      log_tail:
        "2026-10-08T16:02:14Z INFO  bottled: слушаю named pipe \\\\.\\pipe\\bottle\n" +
        "2026-10-08T16:02:15Z INFO  bottled: апплайенс bottleforapps найден, версия 0.2.0\n" +
        "2026-10-08T16:02:15Z INFO  bottled: восстановлено 1 активная бутылка: office-a",
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
