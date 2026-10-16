// Контракт CLI↔GUI (рулинг D1): TS-зеркало `CliReply` из crates/core/src/ipc.rs.
// Имена полей и варианты — ровно как на проводе serde (golden-тесты Task 1
// запинили форму: внешний тег CamelCase, поля snake_case).

/** Результат команды-посредника run_bottle. */
export interface BottleOutput {
  stdout: string;
  code: number;
}

export interface BottleSummary {
  id: string;
  name: string;
  kind: "env" | "host";
  state: string | null;
}

export interface ForwardPort {
  port: number;
  target: string;
}

/** BottleState на проводе: строка-юнит или объект с причиной. */
export type BottleStateWire =
  | string
  | { Degraded: { reason: string } }
  | { Failed: { reason: string } };

export interface RuntimeState {
  id: string;
  state: BottleStateWire;
  socks5: number | null;
  forwards: ForwardPort[];
}

export interface CheckItem {
  code: string;
  status: "Ok" | "Warn" | "Fail";
  detail: string;
}

export type VpnConfigWire =
  | { protocol: "wireguard" | "openvpn"; config: string }
  | {
      client: string;
      args?: string;
      endpoints: string[];
      interface?: string;
    };

export interface BottleProfile {
  id: string;
  name: string;
  kind: "env" | "host";
  vpn: VpnConfigWire;
  expose: {
    socks5: number;
    forwards: { listen: number; target: string }[];
    dns: string | null;
    launch: string | null;
  };
}

export type CliReply =
  | { List: { bottles: BottleSummary[] } }
  | { Show: { profile: BottleProfile | null; runtime: RuntimeState | null } }
  | { Started: { id: string } }
  | { NeedCredentials: { id: string } }
  | { Stopped: { id: string; warning: string | null } }
  | { Status: { bottles: RuntimeState[] } }
  | { Path: string }
  | { DoctorReport: { items: CheckItem[] } }
  | { Created: { id: string } }
  | { Deleted: { id: string; warning: string | null } }
  | { Upgraded: { previous: string | null; current: string } }
  | { Done: { warning: string | null } }
  | { Error: { message: string } };

/** Разбор ответа CLI: stdout → CliReply; ошибки — с русским пояснением. */
export function parseCliReply(output: BottleOutput): CliReply {
  let reply: CliReply;
  try {
    reply = JSON.parse(output.stdout) as CliReply;
  } catch {
    throw new Error(
      `CLI вернул не-JSON ответ (код ${output.code}): ${output.stdout.slice(0, 200)}`,
    );
  }
  if ("Error" in reply) {
    throw new Error(reply.Error.message);
  }
  return reply;
}

/** Признак «демон не запущен» по ошибке обмена (инструкция для пользователя). */
export function isDaemonUnavailable(error: unknown): boolean {
  return (
    error instanceof Error && error.message.includes("запущен ли bottled")
  );
}

// Мост к Rust-команде подставляется извне (main.ts — tauri invoke,
// тесты — заглушка), чтобы модуль оставался тестируемым без Tauri.
export type BottleInvoker = (args: string[]) => Promise<BottleOutput>;
let invoker: BottleInvoker | null = null;

export function setBottleInvoker(fn: BottleInvoker): void {
  invoker = fn;
}

/** Выполняет `bottle --json <args>` и возвращает разобранный CliReply. */
export async function runBottleReply(args: string[]): Promise<CliReply> {
  if (invoker === null) {
    throw new Error("мост к CLI не инициализирован");
  }
  const output = await invoker(args);
  return parseCliReply(output);
}
