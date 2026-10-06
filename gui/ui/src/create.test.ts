// Тесты формы create/edit (M4 Task 4): маппинг полей в аргументы
// `bottle create`, edit через --force с тем же id (M3-семантика),
// ошибки демона — красным под формой (Review Focus #4).

import { beforeEach, describe, expect, it } from "vitest";
import {
  buildCreateArgs,
  fillFormFromProfile,
  readForm,
  submitCreate,
} from "./create";
import { setBottleInvoker } from "./ipc";
import type { BottleProfile } from "./ipc";

/** Минимальная HTML-форма create (атрибуты как в index.html). */
const FORM_HTML = `
<form id="create-form">
  <input id="f-id" />
  <input id="f-name" />
  <select id="f-kind">
    <option value="env">env</option>
    <option value="host">host</option>
  </select>
  <select id="f-protocol">
    <option value="wireguard">wireguard</option>
    <option value="openvpn">openvpn</option>
  </select>
  <input id="f-config" />
  <input id="f-vpn" />
  <input id="f-vpn-args" />
  <input id="f-endpoints" />
  <input id="f-vpn-interface" />
  <input id="f-socks5" type="number" />
  <input id="f-forwards" />
  <input id="f-dns" />
  <input id="f-launch" />
  <button type="submit">Создать</button>
</form>
<p id="create-error" hidden></p>`;

describe("buildCreateArgs", () => {
  it("env-бутылка с полным набором полей", () => {
    expect(
      buildCreateArgs({
        id: "office-a",
        name: "Офис А",
        kind: "env",
        protocol: "wireguard",
        config: "/path/wg.conf",
        socks5: 1080,
        forwards: ["10.20.30.15:3389", "10.20.30.16:3389"],
        dns: "10.20.30.1",
        launch: "mstsc",
      }),
    ).toEqual([
      "create",
      "office-a",
      "--name",
      "Офис А",
      "--kind",
      "env",
      "--protocol",
      "wireguard",
      "--config",
      "/path/wg.conf",
      "--socks5",
      "1080",
      "--forward",
      "10.20.30.15:3389",
      "--forward",
      "10.20.30.16:3389",
      "--dns",
      "10.20.30.1",
      "--launch",
      "mstsc",
    ]);
  });

  it("минимальная env-бутылка: опциональные поля не попадают в args", () => {
    expect(
      buildCreateArgs({
        id: "a",
        name: "A",
        kind: "env",
        protocol: "openvpn",
        config: "/x.ovpn",
        socks5: 0,
        forwards: [],
      }),
    ).toEqual([
      "create",
      "a",
      "--name",
      "A",
      "--kind",
      "env",
      "--protocol",
      "openvpn",
      "--config",
      "/x.ovpn",
    ]);
  });

  it("host-бутылка: --vpn/--endpoint(ы)/--vpn-interface", () => {
    expect(
      buildCreateArgs({
        id: "corp",
        name: "Корп",
        kind: "host",
        vpn: "C:/vpn/vpn.exe",
        vpnArgs: "connect office",
        endpoints: ["vpn1.example.com", "vpn2.example.com"],
        vpnInterface: "Vendor Adapter",
        socks5: 0,
        forwards: [],
      }),
    ).toEqual([
      "create",
      "corp",
      "--name",
      "Корп",
      "--kind",
      "host",
      "--vpn",
      "C:/vpn/vpn.exe",
      "--vpn-args",
      "connect office",
      "--endpoint",
      "vpn1.example.com",
      "--endpoint",
      "vpn2.example.com",
      "--vpn-interface",
      "Vendor Adapter",
    ]);
  });

  it("edit добавляет --force с тем же id (одна операция, M3-семантика)", () => {
    const args = buildCreateArgs(
      {
        id: "office-a",
        name: "Офис А v2",
        kind: "env",
        protocol: "wireguard",
        config: "/new.conf",
        socks5: 0,
        forwards: [],
      },
      true,
    );
    expect(args).toContain("--force");
    expect(args).toContain("office-a");
  });
});

const profile: BottleProfile = {
  id: "office-a",
  name: "Офис А",
  kind: "env",
  vpn: { protocol: "wireguard", config: "wg-office-a.conf" },
  expose: {
    socks5: 1080,
    forwards: [{ listen: 33389, target: "10.20.30.15:3389" }],
    dns: "10.20.30.1",
    launch: "mstsc",
  },
};

describe("edit-путь: show → форма → create --force", () => {
  beforeEach(() => {
    document.body.innerHTML = FORM_HTML;
  });

  it("fillFormFromProfile заполняет поля, id — только чтение", () => {
    fillFormFromProfile(profile);
    const form = readForm();
    expect(form.id).toBe("office-a");
    expect(form.name).toBe("Офис А");
    expect(form.protocol).toBe("wireguard");
    expect(form.socks5).toBe(1080);
    expect(form.forwards).toEqual(["10.20.30.15:3389"]);
    expect(document.querySelector<HTMLInputElement>("#f-id")?.readOnly).toBe(
      true,
    );
  });

  it("форма из профиля + изменение имени → create --force", () => {
    fillFormFromProfile(profile);
    document.querySelector<HTMLInputElement>("#f-name")!.value = "Офис Б";
    const args = buildCreateArgs(readForm(), true);
    expect(args).toContain("--force");
    const nameIndex = args.indexOf("--name");
    expect(args[nameIndex + 1]).toBe("Офис Б");
    // forwards хранит target (listen назначается демоном).
    expect(args).toContain("10.20.30.15:3389");
  });
});

describe("submitCreate", () => {
  beforeEach(() => {
    document.body.innerHTML = FORM_HTML;
  });

  it("успех: ошибка скрыта, список обновится (resolve на Created)", async () => {
    const calls: string[][] = [];
    setBottleInvoker(async (args) => {
      calls.push(args);
      return {
        stdout: JSON.stringify({ Created: { id: "a" } }),
        code: 0,
      };
    });
    fillFormFromProfile(profile);
    document.querySelector<HTMLInputElement>("#f-id")!.readOnly = false;
    const result = await submitCreate(false);
    expect(result).toBe(true);
    expect(calls[0]?.slice(0, 3)).toEqual(["--json", "create", "office-a"]);
    expect(
      document.querySelector<HTMLElement>("#create-error")?.hidden,
    ).toBe(true);
  });

  it("ошибка демона — красным под формой, дословно (Review Focus #4)", async () => {
    setBottleInvoker(async () => ({
      stdout: JSON.stringify({
        Error: { message: "порт 1080 занят процессом mstsc.exe (pid 4242)" },
      }),
      code: 1,
    }));
    const result = await submitCreate(false);
    expect(result).toBe(false);
    const error = document.querySelector<HTMLElement>("#create-error");
    expect(error?.hidden).toBe(false);
    expect(error?.textContent).toContain("порт 1080 занят");
  });
});
