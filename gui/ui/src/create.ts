// Форма create/edit: поля как у `bottle create`; submit =
// `bottle --json create …` — валидация на демоне, его ошибки маппятся
// на конкретное поле по ключевым словам (не распознали — красная строка
// под формой дословно). Клиентская проверка только одна — формат
// идентификатора (DNS-label), чтобы не гонять CLI зря.
// Edit = `create --force` с тем же id (M3-семантика, одна операция).

import { runBottleReply } from "./ipc";
import type { BottleProfile } from "./ipc";

/** Значения формы create. */
export interface CreateFormValues {
  id: string;
  name: string;
  kind: "env" | "host";
  protocol?: "wireguard" | "openvpn";
  config?: string;
  vpn?: string;
  vpnArgs?: string;
  endpoints?: string[];
  vpnInterface?: string;
  socks5: number;
  forwards: string[];
  dns?: string;
  launch?: string;
}

/**
 * Собирает аргументы `bottle create` (без `--json` — его добавляет
 * вызов). Опциональные пустые поля опускаются; forwards и endpoints
 * повторяются флагом; socks5=0 (автоподбор) не передаётся. `force` —
 * edit-режим: перезапись профиля того же id (M3-семантика).
 */
export function buildCreateArgs(values: CreateFormValues, force = false): string[] {
  const args: string[] = ["create", values.id.trim(), "--name", values.name.trim(), "--kind", values.kind];
  if (values.kind === "env") {
    if (values.protocol !== undefined) {
      args.push("--protocol", values.protocol);
    }
    if (values.config !== undefined && values.config !== "") {
      args.push("--config", values.config);
    }
  } else {
    if (values.vpn !== undefined && values.vpn !== "") {
      args.push("--vpn", values.vpn);
    }
    if (values.vpnArgs !== undefined && values.vpnArgs !== "") {
      args.push("--vpn-args", values.vpnArgs);
    }
    for (const endpoint of values.endpoints ?? []) {
      if (endpoint.trim() !== "") {
        args.push("--endpoint", endpoint.trim());
      }
    }
    if (values.vpnInterface !== undefined && values.vpnInterface !== "") {
      args.push("--vpn-interface", values.vpnInterface);
    }
  }
  if (values.socks5 !== 0) {
    args.push("--socks5", String(values.socks5));
  }
  for (const forward of values.forwards) {
    if (forward.trim() !== "") {
      args.push("--forward", forward.trim());
    }
  }
  if (values.dns !== undefined && values.dns !== "") {
    args.push("--dns", values.dns);
  }
  if (values.launch !== undefined && values.launch !== "") {
    args.push("--launch", values.launch);
  }
  // Edit: та же команда с --force — перезапись профиля этого id.
  if (force) {
    args.push("--force");
  }
  return args;
}

/** Читает форму create из DOM (#f-*). */
export function readForm(): CreateFormValues {
  const value = (id: string): string =>
    document.querySelector<HTMLInputElement>(id)?.value ?? "";
  const kind = value("#f-kind") === "host" ? "host" : "env";
  const socks5 = Number.parseInt(value("#f-socks5"), 10);
  const endpoints = value("#f-endpoints")
    .split(/[\s,]+/)
    .filter((item) => item !== "");
  const forwards = value("#f-forwards")
    .split(/[\s,]+/)
    .filter((item) => item !== "");
  const values: CreateFormValues = {
    id: value("#f-id"),
    name: value("#f-name"),
    kind,
    socks5: Number.isNaN(socks5) ? 0 : socks5,
    forwards,
  };
  if (kind === "env") {
    values.protocol = value("#f-protocol") === "openvpn" ? "openvpn" : "wireguard";
    values.config = value("#f-config");
  } else {
    values.vpn = value("#f-vpn");
    values.vpnArgs = value("#f-vpn-args");
    values.endpoints = endpoints;
    values.vpnInterface = value("#f-vpn-interface");
  }
  const dns = value("#f-dns");
  if (dns !== "") {
    values.dns = dns;
  }
  const launch = value("#f-launch");
  if (launch !== "") {
    values.launch = launch;
  }
  return values;
}

/**
 * Заполняет форму из профиля (`show --json`) для edit: все поля, кроме
 * id — он readonly. Пробросы переводятся в targets (listen назначает
 * демон). Конфиг env — имя файла в хранилище; пользователь может
 * выбрать новый файл диалогом (старый остаётся, если поле не трогали).
 */
export function fillFormFromProfile(profile: BottleProfile): void {
  const set = (id: string, value: string): void => {
    const field = document.querySelector<HTMLInputElement>(id);
    if (field !== null) {
      field.value = value;
    }
  };
  set("#f-id", profile.id);
  set("#f-name", profile.name);
  set("#f-kind", profile.kind);
  set("#f-socks5", String(profile.expose.socks5));
  set("#f-forwards", profile.expose.forwards.map((f) => f.target).join(" "));
  set("#f-dns", profile.expose.dns ?? "");
  set("#f-launch", profile.expose.launch ?? "");
  if (profile.kind === "env" && "protocol" in profile.vpn) {
    set("#f-protocol", profile.vpn.protocol);
    set("#f-config", profile.vpn.config);
  }
  if (profile.kind === "host" && "client" in profile.vpn) {
    set("#f-vpn", profile.vpn.client);
    set("#f-vpn-args", profile.vpn.args ?? "");
    set("#f-endpoints", profile.vpn.endpoints.join(" "));
    set("#f-vpn-interface", profile.vpn.interface ?? "");
  }
  const idField = document.querySelector<HTMLInputElement>("#f-id");
  if (idField !== null) {
    idField.readOnly = true;
  }
}

/** Клиентская проверка идентификатора: DNS-label (как имя namespace). */
export function validateBottleId(id: string): string | null {
  if (id.trim() === "") {
    return "Идентификатор обязателен.";
  }
  if (!/^[a-z0-9][a-z0-9-]*$/.test(id.trim())) {
    return "Строчные латинские буквы, цифры и дефис; начинается с буквы или цифры.";
  }
  return null;
}

/**
 * Маппинг ошибки демона на поле формы по ключевым словам сообщения.
 * Возвращает селектор поля или null (покажем общей строкой под формой).
 */
export function fieldForError(message: string): string | null {
  const lower = message.toLowerCase();
  if (lower.includes("socks5") || lower.includes("порт")) {
    return "f-socks5";
  }
  if (lower.includes("конфиг") || lower.includes("config")) {
    return "f-config";
  }
  // «id» — только отдельным словом: «pid 4242» не должно срабатывать.
  if (lower.includes("идентификатор") || /\bid\b/.test(lower)) {
    return "f-id";
  }
  if (lower.includes("dns")) {
    return "f-dns";
  }
  if (lower.includes("проброс") || lower.includes("forward")) {
    return "f-forwards";
  }
  return null;
}

/** Показать ошибку под конкретным полем (.field-error[data-for=…]). */
function showFieldError(fieldId: string, message: string): boolean {
  const error = document.querySelector<HTMLElement>(`.field-error[data-for="${fieldId}"]`);
  const field = document.querySelector<HTMLElement>(`#${fieldId}`);
  if (error === null || field === null) {
    return false;
  }
  error.textContent = message;
  error.hidden = false;
  field.setAttribute("aria-invalid", "true");
  return true;
}

/** Скрыть все полевые ошибки и общую строку. */
function clearErrors(): void {
  for (const error of document.querySelectorAll<HTMLElement>(".field-error")) {
    error.hidden = true;
    error.textContent = "";
  }
  for (const field of document.querySelectorAll<HTMLElement>("#create-form [aria-invalid]")) {
    field.removeAttribute("aria-invalid");
  }
  const general = document.querySelector<HTMLElement>("#create-error");
  if (general !== null) {
    general.hidden = true;
    general.textContent = "";
  }
}

/**
 * Отправляет форму: сначала клиентская проверка id (без CLI-вызова),
 * затем `bottle --json create …` (+--force в edit). Успех — true;
 * ошибка демона маппится на поле по ключевым словам (fieldForError),
 * не распознанная — false и красная строка под формой дословно.
 */
export async function submitCreate(isEdit: boolean): Promise<boolean> {
  clearErrors();
  const values = readForm();
  // В edit id не редактируется — проверять нечего; в create — без вызова CLI.
  if (!isEdit) {
    const idError = validateBottleId(values.id);
    if (idError !== null) {
      showFieldError("f-id", idError);
      return false;
    }
  }
  try {
    await runBottleReply(["--json", ...buildCreateArgs(values, isEdit)]);
    return true;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const fieldId = fieldForError(message);
    if (fieldId !== null && showFieldError(fieldId, message)) {
      return false;
    }
    const errorElement = document.querySelector<HTMLElement>("#create-error");
    if (errorElement !== null) {
      errorElement.hidden = false;
      errorElement.textContent = message;
    }
    return false;
  }
}

/** Загружает профиль в форму для edit: `show --json <id>`. */
export async function loadProfileForEdit(id: string): Promise<boolean> {
  const reply = await runBottleReply(["--json", "show", id]);
  if (!("Show" in reply) || reply.Show.profile === null) {
    return false;
  }
  fillFormFromProfile(reply.Show.profile);
  return true;
}

/** Сброс формы в режим create (новая бутылка, id редактируем). */
export function resetForm(): void {
  const form = document.querySelector<HTMLFormElement>("#create-form");
  form?.reset();
  const idField = document.querySelector<HTMLInputElement>("#f-id");
  if (idField !== null) {
    idField.readOnly = false;
  }
  clearErrors();
}
