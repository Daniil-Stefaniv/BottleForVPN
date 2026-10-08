// Экран списка бутылок: merge `list` + `status` по id, master-detail —
// компактный список слева (точечное обновление DOM по data-bottle-id,
// без пересоздания узлов — фокус и hover не сбрасываются на поллинге),
// панель деталей справа (перерисовка только при смене сигнатуры данных).

import type { BottleStateWire, BottleSummary, RuntimeState } from "./ipc";

/** Строка списка: профиль + рантайм, слитые по id. */
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

/** CSS-класс точки состояния. */
export function stateDotClass(state: string): string {
  const known = ["up", "starting", "stopping", "stopped", "degraded", "failed"];
  return `dot dot-${known.includes(state) ? state : "stopped"}`;
}

export interface ListHandlers {
  onSelect: (id: string) => void;
  onCreate: () => void;
}

/**
 * Точечно обновляет список бутылок в `#bottle-list`: узлы строк живут
 * между поллингами (ключ — data-bottle-id), меняются только классы и
 * тексты; порядок выравнивается append'ом существующих узлов. Счётчик
 * — в `#list-count`.
 */
export function renderBottleList(
  rows: BottleRow[],
  selectedId: string | null,
  handlers: ListHandlers,
): void {
  const list = document.querySelector<HTMLElement>("#bottle-list");
  if (list === null) {
    return;
  }
  const count = document.querySelector<HTMLElement>("#list-count");
  if (count !== null) {
    count.textContent = `Бутылки · ${rows.length}`;
  }
  const existing = new Map<string, HTMLElement>();
  for (const child of list.querySelectorAll<HTMLElement>(".bottle-item")) {
    if (child.dataset.bottleId !== undefined) {
      existing.set(child.dataset.bottleId, child);
    }
  }
  for (const row of rows) {
    let item = existing.get(row.id);
    if (item === undefined) {
      item = buildBottleItem(row, handlers);
    } else {
      existing.delete(row.id);
      updateBottleItem(item, row);
    }
    item.classList.toggle("selected", row.id === selectedId);
    // append переставляет существующий узел — порядок совпадает с rows.
    list.append(item);
  }
  for (const stale of existing.values()) {
    stale.remove();
  }
}

function buildBottleItem(row: BottleRow, handlers: ListHandlers): HTMLElement {
  const item = document.createElement("button");
  item.type = "button";
  item.className = "bottle-item";
  item.dataset.bottleId = row.id;

  const dot = document.createElement("span");
  dot.className = stateDotClass(row.state);

  const body = document.createElement("span");
  body.className = "body";
  const title = document.createElement("span");
  title.className = "title";
  const name = document.createElement("span");
  name.className = "name";
  const kind = document.createElement("span");
  kind.className = "badge";
  title.append(name, kind);
  const sub = document.createElement("span");
  sub.className = "sub";
  body.append(title, sub);
  item.append(dot, body);

  item.addEventListener("click", () => handlers.onSelect(row.id));
  updateBottleItem(item, row);
  return item;
}

function updateBottleItem(item: HTMLElement, row: BottleRow): void {
  const dot = item.querySelector<HTMLElement>(".dot");
  if (dot !== null) {
    dot.className = stateDotClass(row.state);
  }
  const name = item.querySelector<HTMLElement>(".name");
  if (name !== null) {
    name.textContent = row.name === "" ? row.id : row.name;
  }
  const kind = item.querySelector<HTMLElement>(".badge");
  if (kind !== null) {
    kind.textContent = row.kind;
  }
  const sub = item.querySelector<HTMLElement>(".sub");
  if (sub !== null) {
    sub.textContent =
      row.socks5 === null
        ? stateLabel(row.state, row.reason)
        : `SOCKS5 :${row.socks5} · ${stateLabel(row.state, row.reason)}`;
  }
}

export interface DetailHandlers {
  onAction: (id: string, action: BottleAction) => void;
  onEdit: (id: string) => void;
  onDelete: (id: string) => void;
  /** Уведомление (копирование адреса и т.п.). */
  onNotify: (message: string, kind: "success" | "error") => void;
}

/** Сигнатура последнего отрендеренного detail — пропускаем перерисовку,
 * если данные не изменились (не сбиваем выделение текста на поллинге). */
let lastDetailSignature = "";

/** Состояние двухшагового удаления: id бутылки, ждущей подтверждения. */
let deleteConfirmId: string | null = null;

/** Сброс подтверждения удаления (при смене выбранной бутылки). */
export function resetDeleteConfirm(): void {
  deleteConfirmId = null;
}

/** Полный сброс кэша detail-рендера и подтверждения удаления (тесты,
 * смена документа): следующий renderBottleDetail рисует заново. */
export function resetDetailCache(): void {
  lastDetailSignature = "";
  deleteConfirmId = null;
}

/**
 * Рисует панель деталей выбранной бутылки в `#bottle-detail`. Перерисовка
 * только при смене сигнатуры (row + pending + confirm). `pending` —
 * действие, выполняемое прямо сейчас (CLI-вызов): кнопка со спиннером.
 */
export function renderBottleDetail(
  row: BottleRow | null,
  pending: BottleAction | null,
  handlers: DetailHandlers,
): void {
  const pane = document.querySelector<HTMLElement>("#bottle-detail");
  if (pane === null) {
    return;
  }
  const signature = JSON.stringify([row, pending, deleteConfirmId]);
  if (signature === lastDetailSignature) {
    return;
  }
  lastDetailSignature = signature;
  pane.innerHTML = "";

  if (row === null) {
    const empty = document.createElement("p");
    empty.id = "detail-empty";
    empty.className = "muted";
    empty.textContent = "Выберите бутылку слева или создайте новую";
    pane.append(empty);
    return;
  }

  const locked = bottleActionsLocked(row.state) || pending !== null;
  const running = row.state === "up" || row.state === "degraded";

  // Шапка: имя, бейджи состояния/типа, действия.
  const head = document.createElement("div");
  head.className = "detail-head";
  const headText = document.createElement("div");
  const title = document.createElement("h2");
  title.textContent = row.name === "" ? row.id : row.name;
  const badges = document.createElement("div");
  badges.className = "detail-badges";
  const stateBadge = document.createElement("span");
  stateBadge.className = "state-badge";
  const stateDot = document.createElement("span");
  stateDot.className = stateDotClass(row.state);
  const stateText = document.createElement("span");
  stateText.textContent = stateLabel(row.state, row.reason);
  stateBadge.append(stateDot, stateText);
  const kindBadge = document.createElement("span");
  kindBadge.className = "badge";
  kindBadge.textContent = row.kind;
  badges.append(stateBadge, kindBadge);
  headText.append(title, badges);

  const actions = document.createElement("div");
  actions.className = "detail-actions";
  const actionButton = document.createElement("button");
  actionButton.id = "detail-action";
  actionButton.disabled = locked;
  if (pending !== null) {
    const spinner = document.createElement("span");
    spinner.className = "spin";
    spinner.textContent = "◌";
    actionButton.append(spinner, document.createTextNode(" ждите…"));
  } else if (running) {
    actionButton.textContent = "Стоп";
    actionButton.addEventListener("click", () => handlers.onAction(row.id, "stop"));
  } else {
    actionButton.textContent = "Старт";
    actionButton.classList.add("primary");
    actionButton.addEventListener("click", () => handlers.onAction(row.id, "start"));
  }
  const editButton = document.createElement("button");
  editButton.id = "detail-edit";
  editButton.textContent = "Изменить";
  editButton.addEventListener("click", () => handlers.onEdit(row.id));
  const deleteButton = document.createElement("button");
  deleteButton.id = "detail-delete";
  deleteButton.className = "danger";
  if (deleteConfirmId === row.id) {
    deleteButton.textContent = "Подтвердить удаление";
    deleteButton.addEventListener("click", () => {
      deleteConfirmId = null;
      handlers.onDelete(row.id);
    });
  } else {
    deleteButton.textContent = "Удалить";
    deleteButton.addEventListener("click", () => {
      deleteConfirmId = row.id;
      renderBottleDetail(row, pending, handlers);
    });
  }
  actions.append(actionButton, editButton, deleteButton);
  head.append(headText, actions);
  pane.append(head);

  // Карточка параметров.
  const card = document.createElement("div");
  card.className = "detail-card";
  metaRow(card, "Идентификатор", row.id);
  if (row.socks5 !== null) {
    const value = document.createElement("button");
    value.type = "button";
    value.className = "copy mono";
    value.textContent = `127.0.0.1:${row.socks5} ⧉`;
    value.title = "Скопировать адрес SOCKS5";
    value.addEventListener("click", () => {
      void copyText(`127.0.0.1:${row.socks5}`, "Адрес SOCKS5", handlers.onNotify);
    });
    metaRow(card, "SOCKS5-прокси", value);
  } else {
    metaRow(card, "SOCKS5-прокси", "—");
  }
  pane.append(card);

  // Пробросы.
  const forwardsSection = document.createElement("div");
  const forwardsTitle = document.createElement("h3");
  forwardsTitle.className = "detail-section-title";
  forwardsTitle.textContent = "TCP-пробросы";
  forwardsSection.append(forwardsTitle);
  if (row.forwards.length === 0) {
    const none = document.createElement("p");
    none.className = "muted";
    none.textContent = "Не настроены — доступ только через SOCKS5.";
    forwardsSection.append(none);
  } else {
    for (const forward of row.forwards) {
      const line = document.createElement("div");
      line.className = "forward-row";
      const text = document.createElement("span");
      text.className = "mono";
      text.textContent = `127.0.0.1:${forward.port} → ${forward.target}`;
      const copyButton = document.createElement("button");
      copyButton.type = "button";
      copyButton.className = "copy";
      copyButton.textContent = "⧉";
      copyButton.title = "Скопировать адрес проброса";
      copyButton.addEventListener("click", () => {
        void copyText(`127.0.0.1:${forward.port}`, "Адрес проброса", handlers.onNotify);
      });
      line.append(text, copyButton);
      forwardsSection.append(line);
    }
  }
  pane.append(forwardsSection);
}

function metaRow(parent: HTMLElement, label: string, value: string | HTMLElement): void {
  const row = document.createElement("div");
  row.className = "meta-row";
  const labelSpan = document.createElement("span");
  labelSpan.className = "meta-label";
  labelSpan.textContent = label;
  const valueSpan = document.createElement("span");
  valueSpan.className = "meta-value";
  if (typeof value === "string") {
    valueSpan.textContent = value;
  } else {
    valueSpan.append(value);
  }
  row.append(labelSpan, valueSpan);
  parent.append(row);
}

async function copyText(
  text: string,
  what: string,
  notify: (message: string, kind: "success" | "error") => void,
): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    notify(`${what} скопирован: ${text}`, "success");
  } catch {
    notify(`Не удалось скопировать: ${text}`, "error");
  }
}
