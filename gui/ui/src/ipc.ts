// Контракт CLI↔GUI (рулинг D1): TS-зеркало `CliReply` из crates/core/src/ipc.rs.
// Имена полей и варианты — ровно как на проводе serde (golden-тесты Task 1
// запинили форму: внешний тег CamelCase, поля snake_case).

/** Результат команды-посредника run_bottle. */
export interface BottleOutput {
  stdout: string;
  code: number;
}

// --- Проводные типы 0.3 (GUI↔демон, crates/core/src/ipc.rs) --------------

/** Статистика демона (ответ на Stats) для командной панели/Диагностики. */
export interface DaemonStatsWire {
  pid: number;
  version: string;
  started_at: string;
  active_bottles: number;
  total_profiles: number;
  mem_bytes: number;
  cpu_pct: number;
  wsl_state: string;
  appliance_version: string | null;
}

/** Событие журнала (единый формат daemon/gui, спека §6). */
export interface EventWire {
  ts: string;
  level: "debug" | "info" | "warn" | "error" | "fatal";
  src: "daemon" | "gui" | "svc";
  event: string;
  desc: string;
  ctx: Record<string, unknown>;
}

/** Событие демона на event-соединении (push вместо поллинга). */
export type DaemonEventWire =
  | {
      StateChanged: {
        id: string;
        state: string;
        detail: string | null;
        socks5: number | null;
        forwards: ForwardPort[];
      };
    }
  | { Log: EventWire };

export interface BottleSummary {
  id: string;
  name: string;
  kind: "env" | "host";
  state: string | null;
}

/** Строка сетки из команды `list_bottles` (БД, 0.3): сводка + протокол
 * env-бутылки + флаг сохранённых кредов. */
export interface BottleListRowWire {
  id: string;
  name: string;
  kind: "env" | "host";
  protocol: string | null;
  has_credentials: boolean;
  /** Из БД всегда null (состояние приходит событиями демона). */
  state?: string | null;
}

export interface ForwardPort {
  port: number;
  target: string;
}

// --- Окно редактора бутылки (Task 16) ---------------------------------------

/** Данные бутылки для формы редактора (команда `get_bottle`). */
export interface BottleEditorDataWire {
  profile: BottleProfile;
  /** Сохранены ли логин/пароль в хранилище. */
  has_credentials: boolean;
  /** Текст конфига уже в БД (edit без нового файла сохранит его). */
  has_config: boolean;
}

/** Контекст открытого редактора (команда `editor_context`). */
export interface EditorContextWire {
  /** `null` — создание новой бутылки. */
  id: string | null;
  /** Последнее известное состояние (up-бутылка — баннер и запрет удаления). */
  state: string | null;
}

/** Аргументы команды `save_bottle` (форма редактора). */
export interface SaveBottleArgsWire {
  id: string;
  name: string;
  kind: "env" | "host";
  is_edit: boolean;
  protocol?: "wireguard" | "openvpn";
  config_path?: string;
  client?: string;
  args?: string;
  endpoints?: string[];
  interface?: string;
  socks5: number;
  forwards: string[];
  dns?: string;
  launch?: string;
  save_credentials: boolean;
  username?: string;
  password?: string;
}

/** Событие `bottles-changed` (эмитит Rust после мутации БД). */
export interface BottlesChangedWire {
  action: "created" | "updated" | "deleted";
  id: string;
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
