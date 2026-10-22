// Сетка бутылок главного окна (0.3, макет v5): единая сетка
// `minmax(0,1fr) 86px 74px 86px 110px` — бутылка · статус · старт/стоп ·
// изменить · логин и пароль. Merge профилей БД с рантайм-событиями
// демона по id; точечное обновление DOM по data-bottle-id (узлы живут
// между событиями — фокус и hover не сбрасываются).

import type {
  BottleListRowWire,
  BottleStateWire,
  RuntimeState,
} from "./ipc";

/** Строка сетки: профиль БД + рантайм, слитые по id. */
export interface BottleRow {
  id: string;
  name: string;
  kind: "env" | "host";
  /** Протокол env-бутылки ("wireguard"/"openvpn"); у host — null. */
  protocol: string | null;
  /** Сохранены ли логин/пароль в хранилище. */
  hasCredentials: boolean;
  /** Каноническое состояние: up/starting/…/degraded/failed/stopped. */
  state: string;
  /** Причина для degraded/failed (из рантайма), иначе — нет. */
  reason?: string;
  socks5: number | null;
  forwards: { port: number; target: string }[];
}

/** Каноническое имя состояния из проводной формы BottleState. */
export function canonicalState(state: BottleStateWire): string {
  if (typeof state === "string") {
    return state.toLowerCase();
  }
  if ("Degraded" in state) {
    return "degraded";
  }
  return "failed";
}

/** Причина деградации/отказа из проводной формы, если есть. */
export function stateReason(state: BottleStateWire): string | undefined {
  if (typeof state === "string") {
    return undefined;
  }
  return "Degraded" in state ? state.Degraded.reason : state.Failed.reason;
}

/** Русская подпись состояния; для degraded/failed — с причиной. */
export function stateLabel(state: string, reason?: string): string {
  const labels: Record<string, string> = {
    up: "работает",
    starting: "поднимается",
    stopping: "останавливается",
    stopped: "остановлена",
    degraded: "деградация",
    failed: "ошибка",
  };
  const base = labels[state] ?? state;
  return reason === undefined ? base : `${base}: ${reason}`;
}

/** Переходные состояния блокируют кнопки старт/стоп. */
export function bottleActionsLocked(state: string): boolean {
  return state === "starting" || state === "stopping";
}

/**
 * Сливает профили БД и рантайм-снимок демона по id: рантайм важнее —
 * из него состояние, SOCKS5-порт и пробросы. Бутылка в рантайме без
 * профиля (сирота) тоже показывается.
 */
export function mergeListStatus(
  bottles: BottleListRowWire[],
  runtime: RuntimeState[],
): BottleRow[] {
  const byId = new Map(runtime.map((state) => [state.id, state]));
  const rows: BottleRow[] = bottles.map((bottle) => {
    const live = byId.get(bottle.id);
    byId.delete(bottle.id);
    return {
      id: bottle.id,
      name: bottle.name,
      kind: bottle.kind,
      protocol: bottle.protocol,
      hasCredentials: bottle.has_credentials,
      state: live
        ? canonicalState(live.state)
        : (bottle.state?.toLowerCase() ?? "stopped"),
      reason: live === undefined ? undefined : stateReason(live.state),
      socks5: live?.socks5 ?? null,
      forwards: live?.forwards ?? [],
    };
  });
  // Сироты: рантайм есть, профиля нет.
  for (const live of byId.values()) {
    rows.push({
      id: live.id,
      name: "",
      kind: "env",
      protocol: null,
      hasCredentials: false,
      state: canonicalState(live.state),
      reason: stateReason(live.state),
      socks5: live.socks5,
      forwards: live.forwards,
    });
  }
  return rows;
}

/** Действие кнопки в строке бутылки. */
export type BottleAction = "start" | "stop";

export interface GridHandlers {
  onAction: (id: string, action: BottleAction) => void;
  onEdit: (id: string) => void;
  /** «Логин и пароль» (пункт 9): окно обновления кредов. */
  onCreds: (id: string) => void;
}

/** Мета-строка карточки: протокол/клиент, адреса, причина, креды. */
export function rowMeta(row: BottleRow, daemonDown: boolean): string {
  if (daemonDown) {
    return "демон выкл.";
  }
  const parts: string[] = [];
  if (row.kind === "env") {
    parts.push(row.protocol === "openvpn" ? "OpenVPN" : "WireGuard");
  } else {
    parts.push("host-клиент");
  }
  if (row.socks5 !== null) {
    parts.push(`socks5 :${row.socks5}`);
  }
  for (const forward of row.forwards) {
    parts.push(`:${forward.port}→${forward.target}`);
  }
  if (row.reason !== undefined) {
    parts.push(row.reason);
  }
  if (row.protocol === "openvpn" && row.hasCredentials) {
    parts.push("сохранён логин/пароль");
  }
  return parts.join(" · ");
}

/** CSS-класс пилюли состояния. */
export function statePillClass(state: string): string {
  const map: Record<string, string> = {
    up: "pill pill-up",
    starting: "pill pill-transit",
    stopping: "pill pill-transit",
    degraded: "pill pill-deg",
    failed: "pill pill-deg",
    stopped: "pill pill-down",
  };
  return map[state] ?? "pill pill-down";
}

/**
 * Точечно обновляет сетку в `#bottle-grid`: узлы строк живут между
 * обновлениями (ключ — data-bottle-id), порядок выравнивается append'ом.
 * `daemonDown` гасит старт/стоп у всех строк; `pending` — спиннер
 * действия по id.
 */
export function renderBottleGrid(
  rows: BottleRow[],
  daemonDown: boolean,
  pending: ReadonlyMap<string, BottleAction>,
  handlers: GridHandlers,
): void {
  const grid = document.querySelector<HTMLElement>("#bottle-grid");
  if (grid === null) {
    return;
  }
  const empty = document.querySelector<HTMLElement>("#grid-empty");
  if (empty !== null) {
    empty.hidden = rows.length !== 0;
  }
  const existing = new Map<string, HTMLElement>();
  for (const child of grid.querySelectorAll<HTMLElement>(".grid-row")) {
    if (child.dataset.bottleId !== undefined) {
      existing.set(child.dataset.bottleId, child);
    }
  }
  for (const row of rows) {
    let item = existing.get(row.id);
    if (item === undefined) {
      item = buildGridRow(row, handlers);
    } else {
      existing.delete(row.id);
    }
    updateGridRow(item, row, daemonDown, pending.get(row.id) ?? null);
    grid.append(item);
  }
  for (const stale of existing.values()) {
    stale.remove();
  }
}

function buildGridRow(row: BottleRow, handlers: GridHandlers): HTMLElement {
  const item = document.createElement("div");
  item.className = "grid-row";
  item.dataset.bottleId = row.id;

  const main = document.createElement("span");
  main.className = "grid-main";
  const name = document.createElement("span");
  name.className = "grid-name";
  const meta = document.createElement("span");
  meta.className = "grid-meta";
  main.append(name, meta);

  const pill = document.createElement("span");
  pill.className = "pill pill-down";
  pill.dataset.role = "state";

  const action = document.createElement("button");
  action.type = "button";
  action.dataset.role = "action";
  action.addEventListener("click", () => {
    const kind = action.dataset.action === "stop" ? "stop" : "start";
    handlers.onAction(row.id, kind);
  });

  const edit = document.createElement("button");
  edit.type = "button";
  edit.className = "grid-btn";
  edit.dataset.role = "edit";
  edit.textContent = "Изменить";
  edit.addEventListener("click", () => handlers.onEdit(row.id));

  const creds = document.createElement("button");
  creds.type = "button";
  creds.className = "grid-btn";
  creds.dataset.role = "creds";
  creds.textContent = "Логин и пароль";
  creds.addEventListener("click", () => handlers.onCreds(row.id));

  item.append(main, pill, action, edit, creds);
  return item;
}

function updateGridRow(
  item: HTMLElement,
  row: BottleRow,
  daemonDown: boolean,
  pending: BottleAction | null,
): void {
  const name = item.querySelector<HTMLElement>(".grid-name");
  if (name !== null) {
    name.textContent = row.name === "" ? row.id : row.name;
    name.title = row.id;
  }
  const meta = item.querySelector<HTMLElement>(".grid-meta");
  if (meta !== null) {
    meta.textContent = rowMeta(row, daemonDown);
  }
  const pill = item.querySelector<HTMLElement>("[data-role=state]");
  if (pill !== null) {
    pill.className = statePillClass(row.state);
    pill.textContent = daemonDown ? "—" : row.state;
    if (!daemonDown && row.reason !== undefined) {
      pill.title = row.reason;
    } else {
      pill.removeAttribute("title");
    }
  }

  const action = item.querySelector<HTMLButtonElement>("[data-role=action]");
  if (action !== null) {
    const running = row.state === "up" || row.state === "degraded";
    const locked = bottleActionsLocked(row.state) || pending !== null;
    action.dataset.action = running ? "stop" : "start";
    action.classList.remove("go", "halt", "off");
    if (daemonDown) {
      action.textContent = "Старт";
      action.classList.add("off");
      action.title = "демон не запущен";
      action.disabled = true;
    } else if (pending !== null) {
      action.textContent = "ждите…";
      action.classList.add("off");
      action.title = "";
      action.disabled = true;
    } else if (locked) {
      action.textContent = running ? "Стоп" : "Старт";
      action.classList.add("off");
      action.title = "идёт переход состояния";
      action.disabled = true;
    } else if (running) {
      action.textContent = "Стоп";
      action.classList.add("halt");
      action.title = "";
      action.disabled = false;
    } else {
      action.textContent = "Старт";
      action.classList.add("go");
      action.title = "";
      action.disabled = false;
    }
  }

  const creds = item.querySelector<HTMLButtonElement>("[data-role=creds]");
  if (creds !== null) {
    const unsupported =
      row.kind === "host"
        ? "У host-бутылки логин и пароль задаёт фирменный клиент"
        : row.protocol === "wireguard"
          ? "У WireGuard нет логина/пароля"
          : null;
    creds.disabled = unsupported !== null;
    creds.classList.toggle("off", unsupported !== null);
    if (unsupported !== null) {
      creds.title = unsupported;
    } else {
      creds.title = "Обновить логин и пароль";
    }
  }
}
