// Форма create/edit (M4 Task 4): поля как у `bottle create`; submit =
// `bottle --json create …` — валидация на демоне, его ошибки показываются
// красным под формой дословно (Review Focus #4: клиентских правил нет).
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

/**
 * Отправляет форму: `bottle --json create …` (+--force в edit).
 * Успех — true и скрытая ошибка; ошибка демона — false и красная
 * строка под формой с его сообщением дословно.
 */
export async function submitCreate(isEdit: boolean): Promise<boolean> {
  const values = readForm();
  const errorElement = document.querySelector<HTMLElement>("#create-error");
  try {
    await runBottleReply(["--json", ...buildCreateArgs(values, isEdit)]);
    if (errorElement !== null) {
      errorElement.hidden = true;
      errorElement.textContent = "";
    }
    return true;
  } catch (error) {
    if (errorElement !== null) {
      errorElement.hidden = false;
      errorElement.textContent =
        error instanceof Error ? error.message : String(error);
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
  const errorElement = document.querySelector<HTMLElement>("#create-error");
  if (errorElement !== null) {
    errorElement.hidden = true;
  }
}
