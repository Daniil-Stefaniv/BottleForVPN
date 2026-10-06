// Экран списка бутылок (M4 Task 3): merge `list` + `status` по id,
// таблица id/name/kind/state/socks5/forwards, кнопки Старт/Стоп с
// блокировкой на время переходов.

import type { BottleStateWire, BottleSummary, RuntimeState } from "./ipc";

/** Строка таблицы: профиль + рантайм, слитые по id. */
export interface BottleRow {
  id: string;
  name: string;
  kind: "env" | "host";
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
 * Сливает `bottle list` (профили) и `bottle status` (рантайм) по id:
 * рантайм важнее — из него состояние, SOCKS5-порт и пробросы. Бутылка
 * в status без профиля (сирота T13) тоже показывается.
 */
export function mergeListStatus(
  bottles: BottleSummary[],
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

/**
 * Рисует таблицу бутылок в `#bottles` (создаёт thead/tbody): колонки
 * id, имя, тип, состояние, SOCKS5, пробросы; кнопки Старт/Стоп
 * заблокированы в переходных состояниях.
 */
export function renderListTable(
  rows: BottleRow[],
  onAction: (id: string, action: BottleAction) => void,
): void {
  const table = document.querySelector<HTMLTableElement>("#bottles");
  if (table === null) {
    return;
  }
  table.innerHTML = "";
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const title of ["ID", "Имя", "Тип", "Состояние", "SOCKS5", "Пробросы", ""]) {
    const th = document.createElement("th");
    th.textContent = title;
    headRow.append(th);
  }
  thead.append(headRow);
  const tbody = document.createElement("tbody");
  for (const row of rows) {
    const tr = document.createElement("tr");
    tr.dataset.bottleId = row.id;

    const idCell = document.createElement("td");
    idCell.textContent = row.id;
    const nameCell = document.createElement("td");
    nameCell.textContent = row.name;
    const kindCell = document.createElement("td");
    kindCell.textContent = row.kind;
    const stateCell = document.createElement("td");
    stateCell.textContent = stateLabel(row.state, row.reason);
    const socksCell = document.createElement("td");
    socksCell.textContent = row.socks5 === null ? "—" : String(row.socks5);
    const forwardsCell = document.createElement("td");
    forwardsCell.textContent =
      row.forwards.length === 0
        ? "—"
        : row.forwards.map((f) => `${f.port}→${f.target}`).join(", ");

    const actionsCell = document.createElement("td");
    actionsCell.className = "actions";
    const locked = bottleActionsLocked(row.state);
    const startButton = document.createElement("button");
    startButton.textContent = "Старт";
    startButton.disabled = locked;
    startButton.addEventListener("click", () => onAction(row.id, "start"));
    const stopButton = document.createElement("button");
    stopButton.textContent = "Стоп";
    stopButton.disabled = locked;
    stopButton.addEventListener("click", () => onAction(row.id, "stop"));
    actionsCell.append(startButton, stopButton);

    tr.append(
      idCell,
      nameCell,
      kindCell,
      stateCell,
      socksCell,
      forwardsCell,
      actionsCell,
    );
    tbody.append(tr);
  }
  table.append(thead, tbody);
}
