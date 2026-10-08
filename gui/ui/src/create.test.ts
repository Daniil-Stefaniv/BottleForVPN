// Тесты формы create/edit: маппинг полей в аргументы `bottle create`,
// edit через --force с тем же id (M3-семантика), клиентская проверка
// идентификатора, ошибки демона — на конкретное поле по ключевым словам
// (не распознанные — общей строкой под формой, дословно).

import { beforeEach, describe, expect, it } from "vitest";
import {
  buildCreateArgs,
  fieldForError,
  fillFormFromProfile,
  readForm,
  submitCreate,
  validateBottleId,
} from "./create";
import { setBottleInvoker } from "./ipc";
import type { BottleProfile } from "./ipc";

/** Минимальная HTML-форма create (атрибуты как в index.html). */
const FORM_HTML = `
<form id="create-form">
  <input id="f-id" />
  <p class="field-error" data-for="f-id" hidden></p>
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
  <p class="field-error" data-for="f-config" hidden></p>
  <input id="f-vpn" />
  <input id="f-vpn-args" />
  <input id="f-endpoints" />
  <input id="f-vpn-interface" />
  <input id="f-socks5" type="number" />
  <p class="field-error" data-for="f-socks5" hidden></p>
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

describe("validateBottleId (клиентская проверка)", () => {
  it("принимает DNS-label", () => {
    expect(validateBottleId("office-a")).toBeNull();
    expect(validateBottleId("a1")).toBeNull();
  });

  it("отклоняет пустой и недопустимые символы", () => {
    expect(validateBottleId("")).not.toBeNull();
    expect(validateBottleId("  ")).not.toBeNull();
    expect(validateBottleId("Office_A")).not.toBeNull();
    expect(validateBottleId("-office")).not.toBeNull();
  });
});

describe("fieldForError (маппинг ошибок демона на поля)", () => {
  it("порт/socks5 → f-socks5, конфиг → f-config, идентификатор → f-id", () => {
    expect(fieldForError("порт 1080 занят процессом mstsc.exe")).toBe("f-socks5");
    expect(fieldForError("файл конфигурации не найден")).toBe("f-config");
    expect(fieldForError("идентификатор занят")).toBe("f-id");
    expect(fieldForError("некорректный DNS-сервер")).toBe("f-dns");
  });

  it("нераспознанная ошибка — null (общая строка под формой)", () => {
    expect(fieldForError("хранилище профилей недоступно")).toBeNull();
  });
});

describe("submitCreate", () => {
  beforeEach(() => {
    document.body.innerHTML = FORM_HTML;
  });

  it("успех: ошибки скрыты, список обновится (resolve на Created)", async () => {
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

  it("клиентская проверка: невалидный id — ошибка у поля, CLI не вызывается", async () => {
    let cliCalled = false;
    setBottleInvoker(async () => {
      cliCalled = true;
      return { stdout: "{}", code: 0 };
    });
    document.querySelector<HTMLInputElement>("#f-id")!.value = "Bad_ID!";
    const result = await submitCreate(false);
    expect(result).toBe(false);
    expect(cliCalled).toBe(false);
    const fieldError = document.querySelector<HTMLElement>(
      '.field-error[data-for="f-id"]',
    );
    expect(fieldError?.hidden).toBe(false);
    expect(fieldError?.textContent).toContain("Строчные");
    expect(
      document.querySelector<HTMLInputElement>("#f-id")?.getAttribute("aria-invalid"),
    ).toBe("true");
  });

  it("ошибка демона про порт маппится на поле SOCKS5, дословно", async () => {
    setBottleInvoker(async () => ({
      stdout: JSON.stringify({
        Error: { message: "порт 1080 занят процессом mstsc.exe (pid 4242)" },
      }),
      code: 1,
    }));
    document.querySelector<HTMLInputElement>("#f-id")!.value = "office-a";
    const result = await submitCreate(false);
    expect(result).toBe(false);
    const fieldError = document.querySelector<HTMLElement>(
      '.field-error[data-for="f-socks5"]',
    );
    expect(fieldError?.hidden).toBe(false);
    expect(fieldError?.textContent).toContain("порт 1080 занят");
    // Общая строка остаётся скрытой — ошибка ушла в поле.
    expect(
      document.querySelector<HTMLElement>("#create-error")?.hidden,
    ).toBe(true);
  });

  it("нераспознанная ошибка демона — общей строкой под формой, дословно", async () => {
    setBottleInvoker(async () => ({
      stdout: JSON.stringify({
        Error: { message: "хранилище профилей недоступно" },
      }),
      code: 1,
    }));
    document.querySelector<HTMLInputElement>("#f-id")!.value = "office-a";
    const result = await submitCreate(false);
    expect(result).toBe(false);
    const error = document.querySelector<HTMLElement>("#create-error");
    expect(error?.hidden).toBe(false);
    expect(error?.textContent).toContain("хранилище профилей недоступно");
  });

  it("повторный submit после ошибки очищает прошлые подсветки", async () => {
    let attempt = 0;
    setBottleInvoker(async () => {
      attempt += 1;
      if (attempt === 1) {
        return {
          stdout: JSON.stringify({
            Error: { message: "порт 1080 занят" },
          }),
          code: 1,
        };
      }
      return { stdout: JSON.stringify({ Created: { id: "a" } }), code: 0 };
    });
    document.querySelector<HTMLInputElement>("#f-id")!.value = "office-a";
    await submitCreate(false);
    const result = await submitCreate(false);
    expect(result).toBe(true);
    const fieldError = document.querySelector<HTMLElement>(
      '.field-error[data-for="f-socks5"]',
    );
    expect(fieldError?.hidden).toBe(true);
    expect(
      document.querySelector<HTMLInputElement>("#f-socks5")?.getAttribute("aria-invalid"),
    ).toBeNull();
  });
});
