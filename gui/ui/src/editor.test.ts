// Тесты редактора бутылки (Task 16, бриф Step 1): валидация и
// автогенерация id, переключатель env/host меняет поля, чекбокс кредов
// только при OpenVPN (второй шаг), дубль id — ошибкой в поле, баннер
// «после перезапуска» и запрет удаления у поднятой бутылки, удаление
// с подтверждением. Мост GuiApi и окружение окна — заглушки.

import { afterEach, describe, expect, it, vi } from "vitest";
import type { GuiApi } from "./backend";
import type {
  BottleEditorDataWire,
  EditorContextWire,
  SaveBottleArgsWire,
} from "./ipc";

function makeGuiApi(overrides: Partial<GuiApi> = {}): GuiApi {
  return {
    listBottles: vi.fn(async () => []),
    daemonConnect: vi.fn(async () => {}),
    startBottle: vi.fn(async () => {}),
    stopBottle: vi.fn(async () => {}),
    stopDaemon: vi.fn(async () => {}),
    daemonStats: vi.fn(async () => {
      throw new Error("не нужен в тестах редактора");
    }),
    guiLogRecent: vi.fn(async () => []),
    logsDir: vi.fn(async () => "/tmp/pogrebok-logs"),
    updateCredentials: vi.fn(async () => {}),
    logEvent: vi.fn(async () => {}),
    openEditor: vi.fn(async () => {}),
    getBottle: vi.fn(async () => null),
    saveBottle: vi.fn(async () => {}),
    deleteBottle: vi.fn(async () => {}),
    ...overrides,
  };
}

function makeHost(context: EditorContextWire) {
  return {
    context: vi.fn(async () => context),
    pickConfig: vi.fn(async () => null as string | null),
    close: vi.fn(),
  };
}

/** Свежие модули + контейнер; module-level host/api не текут между тестами. */
async function boot(context: EditorContextWire, guiOverrides: Partial<GuiApi> = {}) {
  vi.resetModules();
  const editor = await import("./editor");
  const backend = await import("./backend");
  const api = makeGuiApi(guiOverrides);
  backend.setGuiApi(api);
  const host = makeHost(context);
  editor.setEditorHost(host);
  const root = document.createElement("div");
  document.body.append(root);
  await editor.mountEditor(root);
  return { editor, api, host, root };
}

function input(root: HTMLElement, selector: string, value: string): void {
  const field = root.querySelector<HTMLInputElement>(selector);
  if (field === null) {
    throw new Error(`нет поля ${selector}`);
  }
  field.value = value;
  field.dispatchEvent(new Event("input"));
}

function submit(root: HTMLElement): void {
  root
    .querySelector<HTMLFormElement>("#editor-form")
    ?.dispatchEvent(new Event("submit", { cancelable: true }));
}

async function until(check: () => void): Promise<void> {
  await vi.waitFor(check, { timeout: 1000, interval: 10 });
}

const EDITOR_DATA: BottleEditorDataWire = {
  profile: {
    id: "office-a",
    name: "Офис А",
    kind: "env",
    vpn: { protocol: "wireguard", config: "wg-office-a.conf" },
    expose: {
      socks5: 55123,
      forwards: [{ listen: 13389, target: "10.20.30.15:3389" }],
      dns: "10.20.30.1",
      launch: "mstsc",
    },
  },
  has_credentials: false,
  has_config: true,
};

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("чистые функции редактора", () => {
  it("validateBottleId: пустой, заглавные и ведущий дефис — ошибки", async () => {
    const { editor } = await boot({ id: null, state: null });
    expect(editor.validateBottleId("")).not.toBeNull();
    expect(editor.validateBottleId("Office")).not.toBeNull();
    expect(editor.validateBottleId("-office")).not.toBeNull();
    expect(editor.validateBottleId("офис")).not.toBeNull();
    expect(editor.validateBottleId("office-a")).toBeNull();
    expect(editor.validateBottleId("a1")).toBeNull();
  });

  it("suggestId: транслит кириллицы, схлопывание дефисов", async () => {
    const { editor } = await boot({ id: null, state: null });
    expect(editor.suggestId("Офис Б")).toBe("ofis-b");
    expect(editor.suggestId("VPN-Corp 2")).toBe("vpn-corp-2");
    expect(editor.suggestId("Ёлка Щука")).toBe("elka-schuka");
    expect(editor.suggestId(" — пробелы — ")).toBe("probely");
    expect(editor.suggestId("")).toBe("");
  });

  it("fieldForError: дубль id — на поле идентификатора", async () => {
    const { editor } = await boot({ id: null, state: null });
    expect(
      editor.fieldForError("бутылка с id «office-a» уже существует — выберите другой id"),
    ).toBe("e-id");
    expect(editor.fieldForError("укажите файл конфигурации туннеля")).toBe("e-config");
    expect(editor.fieldForError("проброс «x»: ожидается формат host:port")).toBe(
      "e-forwards",
    );
    expect(editor.fieldForError("что-то незнакомое")).toBeNull();
  });
});

describe("форма создания", () => {
  it("id генерируется из имени, пока пользователь не тронул поле", async () => {
    const { root } = await boot({ id: null, state: null });
    input(root, "#e-name", "Офис Б");
    expect(root.querySelector<HTMLInputElement>("#e-id")?.value).toBe("ofis-b");
    input(root, "#e-id", "custom-id");
    input(root, "#e-name", "Другой офис");
    expect(root.querySelector<HTMLInputElement>("#e-id")?.value).toBe("custom-id");
  });

  it("переключатель env/host меняет набор полей", async () => {
    const { root } = await boot({ id: null, state: null });
    expect(
      root.querySelector<HTMLElement>("#e-section-env")?.hidden,
    ).toBe(false);
    root.querySelector<HTMLButtonElement>('[data-kind="host"]')?.click();
    expect(root.querySelector<HTMLElement>("#e-section-env")?.hidden).toBe(true);
    expect(root.querySelector<HTMLElement>("#e-section-host")?.hidden).toBe(false);
    root.querySelector<HTMLButtonElement>('[data-kind="env"]')?.click();
    expect(root.querySelector<HTMLElement>("#e-section-env")?.hidden).toBe(false);
    expect(root.querySelector<HTMLElement>("#e-section-host")?.hidden).toBe(true);
  });

  it("чекбокс кредов виден только при OpenVPN", async () => {
    const { root } = await boot({ id: null, state: null });
    const row = root.querySelector<HTMLElement>("#e-creds-check-row");
    expect(row?.hidden).toBe(true); // wireguard по умолчанию
    const protocol = root.querySelector<HTMLSelectElement>("#e-protocol");
    if (protocol !== null) {
      protocol.value = "openvpn";
      protocol.dispatchEvent(new Event("change"));
    }
    expect(row?.hidden).toBe(false);
    root.querySelector<HTMLButtonElement>('[data-kind="host"]')?.click();
    expect(row?.hidden).toBe(true);
  });

  it("без файла конфига — ошибка у поля, сохранения нет", async () => {
    const { root, api } = await boot({ id: null, state: null });
    input(root, "#e-name", "Офис Б");
    submit(root);
    const error = root.querySelector<HTMLElement>('.field-error[data-for="e-config"]');
    expect(error?.hidden).toBe(false);
    expect(error?.textContent).toContain("конфигурации");
    expect(api.saveBottle).not.toHaveBeenCalled();
  });

  it("валидная wireguard-бутылка сохраняется и закрывает окно", async () => {
    const { root, api, host } = await boot({ id: null, state: null });
    input(root, "#e-name", "Офис Б");
    input(root, "#e-config", "C:\\vpn\\wg-office-b.conf");
    input(root, "#e-forwards", "10.20.30.15:3389");
    submit(root);
    await until(() => {
      expect(api.saveBottle).toHaveBeenCalledTimes(1);
    });
    const args = (api.saveBottle as ReturnType<typeof vi.fn>).mock
      .calls[0]?.[0] as SaveBottleArgsWire;
    expect(args).toMatchObject({
      id: "ofis-b",
      name: "Офис Б",
      kind: "env",
      is_edit: false,
      protocol: "wireguard",
      config_path: "C:\\vpn\\wg-office-b.conf",
      forwards: ["10.20.30.15:3389"],
      save_credentials: false,
    });
    await until(() => {
      expect(host.close).toHaveBeenCalledTimes(1);
    });
  });

  it("дубль id — ошибка из БД рендерится в поле идентификатора", async () => {
    const { root, host } = await boot({ id: null, state: null }, {
      saveBottle: vi.fn(async () => {
        throw new Error("бутылка с id «ofis-b» уже существует — выберите другой id");
      }),
    });
    input(root, "#e-name", "Офис Б");
    input(root, "#e-config", "C:\\vpn\\wg.conf");
    submit(root);
    await until(() => {
      const error = root.querySelector<HTMLElement>('.field-error[data-for="e-id"]');
      expect(error?.hidden).toBe(false);
    });
    expect(
      root.querySelector<HTMLElement>('.field-error[data-for="e-id"]')?.textContent,
    ).toContain("уже существует");
    expect(host.close).not.toHaveBeenCalled();
  });

  it("OpenVPN + чекбокс: второй шаг спрашивает логин/пароль", async () => {
    const { root, api, host } = await boot({ id: null, state: null });
    input(root, "#e-name", "Офис Б");
    const protocol = root.querySelector<HTMLSelectElement>("#e-protocol");
    if (protocol !== null) {
      protocol.value = "openvpn";
      protocol.dispatchEvent(new Event("change"));
    }
    input(root, "#e-config", "C:\\vpn\\office-b.ovpn");
    root.querySelector<HTMLInputElement>("#e-save-creds")?.click();
    submit(root);
    // Первый шаг ушёл, второй — на экране; сохранения ещё не было.
    expect(root.querySelector<HTMLElement>("#editor-form")?.hidden).toBe(true);
    expect(root.querySelector<HTMLElement>("#e-step-creds")?.hidden).toBe(false);
    expect(api.saveBottle).not.toHaveBeenCalled();

    // Пустой логин не принимается.
    root.querySelector<HTMLButtonElement>("#e-creds-save")?.click();
    expect(root.querySelector<HTMLElement>("#e-creds-error")?.textContent).toContain(
      "логин",
    );
    expect(api.saveBottle).not.toHaveBeenCalled();

    input(root, "#e-username", "ivan.petrov");
    input(root, "#e-password", "s3cret");
    root.querySelector<HTMLButtonElement>("#e-creds-save")?.click();
    await until(() => {
      expect(api.saveBottle).toHaveBeenCalledTimes(1);
    });
    const args = (api.saveBottle as ReturnType<typeof vi.fn>).mock
      .calls[0]?.[0] as SaveBottleArgsWire;
    expect(args).toMatchObject({
      id: "ofis-b",
      protocol: "openvpn",
      save_credentials: true,
      username: "ivan.petrov",
      password: "s3cret",
    });
    await until(() => {
      expect(host.close).toHaveBeenCalledTimes(1);
    });
  });
});

describe("форма редактирования", () => {
  it("поднятая бутылка: баннер, удаление запрещено, конфиг сохранён", async () => {
    const { root, api, host } = await boot(
      { id: "office-a", state: "up" },
      { getBottle: vi.fn(async () => EDITOR_DATA) },
    );
    expect(root.querySelector<HTMLElement>("#e-title")?.textContent).toBe(
      "Изменить «Офис А»",
    );
    expect(root.querySelector<HTMLInputElement>("#e-id")?.readOnly).toBe(true);
    expect(root.querySelector<HTMLElement>("#e-banner")?.hidden).toBe(false);
    const deleteBtn = root.querySelector<HTMLButtonElement>("#e-delete");
    expect(deleteBtn?.hidden).toBe(false);
    expect(deleteBtn?.disabled).toBe(true);
    expect(deleteBtn?.title).toBe("Сначала остановите бутылку");
    // Подсказка о сохранённом конфиге; поле файла пустое.
    expect(root.querySelector<HTMLElement>("#e-config-hint")?.hidden).toBe(false);
    expect(root.querySelector<HTMLInputElement>("#e-config")?.value).toBe("");

    // Сохранение без перевыбора файла: config_path не уходит.
    input(root, "#e-name", "Офис А (переименован)");
    submit(root);
    await until(() => {
      expect(api.saveBottle).toHaveBeenCalledTimes(1);
    });
    const args = (api.saveBottle as ReturnType<typeof vi.fn>).mock
      .calls[0]?.[0] as SaveBottleArgsWire;
    expect(args.is_edit).toBe(true);
    expect(args.config_path).toBeUndefined();
    expect(args.socks5).toBe(55123);
    await until(() => {
      expect(host.close).toHaveBeenCalledTimes(1);
    });
  });

  it("остановленная: удаление с подтверждением уходит в мост", async () => {
    const { editor, root, api, host } = await boot(
      { id: "office-a", state: "stopped" },
      { getBottle: vi.fn(async () => EDITOR_DATA) },
    );
    editor.setEditorConfirm(() => true);
    const deleteBtn = root.querySelector<HTMLButtonElement>("#e-delete");
    expect(deleteBtn?.disabled).toBe(false);
    expect(root.querySelector<HTMLElement>("#e-banner")?.hidden).toBe(true);
    deleteBtn?.click();
    await until(() => {
      expect(api.deleteBottle).toHaveBeenCalledWith("office-a");
    });
    await until(() => {
      expect(host.close).toHaveBeenCalledTimes(1);
    });
  });

  it("отмена подтверждения — удаления нет", async () => {
    const { editor, root, api, host } = await boot(
      { id: "office-a", state: "stopped" },
      { getBottle: vi.fn(async () => EDITOR_DATA) },
    );
    editor.setEditorConfirm(() => false);
    root.querySelector<HTMLButtonElement>("#e-delete")?.click();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(api.deleteBottle).not.toHaveBeenCalled();
    expect(host.close).not.toHaveBeenCalled();
  });

  it("host-бутылка: поля клиента заполнены, секция env скрыта", async () => {
    const hostData: BottleEditorDataWire = {
      profile: {
        id: "legacy-ac",
        name: "Legacy AnyConnect",
        kind: "host",
        vpn: {
          client: "C:\\vpn\\anyconnect.exe",
          args: "connect",
          endpoints: ["vpn.example.com"],
        },
        expose: { socks5: 0, forwards: [], dns: null, launch: null },
      },
      has_credentials: false,
      has_config: false,
    };
    const { root } = await boot(
      { id: "legacy-ac", state: "stopped" },
      { getBottle: vi.fn(async () => hostData) },
    );
    expect(root.querySelector<HTMLElement>("#e-section-host")?.hidden).toBe(false);
    expect(root.querySelector<HTMLElement>("#e-section-env")?.hidden).toBe(true);
    expect(root.querySelector<HTMLInputElement>("#e-vpn")?.value).toBe(
      "C:\\vpn\\anyconnect.exe",
    );
    expect(root.querySelector<HTMLInputElement>("#e-endpoints")?.value).toBe(
      "vpn.example.com",
    );
  });
});
