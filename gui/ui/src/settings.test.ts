// Тесты настроек (Task 18, спека §3.1/§9): смена мастер-пароля (старый
// проверяется на Rust-стороне; «wrong_password» — ошибкой под полем),
// горячие клавиши (Ctrl+, настройки, Ctrl+L Диагностика, F5 обновить —
// раскладко-независимо по event.code), состояние окна в meta-ключах ui.*:
// размер восстанавливается, вкладка возвращается, мусор игнорируется.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  installHotkeys,
  installWindowSizePersist,
  persistTab,
  restoreWindowState,
  showSettings,
  UI_TAB_KEY,
  UI_WIN_H_KEY,
  UI_WIN_W_KEY,
  type SettingsDeps,
  type WindowStateDeps,
} from "./settings";

const HTML = `
<main id="app">
  <dialog id="settings-dialog">
    <form id="settings-form">
      <input id="settings-old" type="password" />
      <input id="settings-new" type="password" />
      <input id="settings-new2" type="password" />
      <p id="settings-error" hidden></p>
      <code id="settings-data-dir"></code>
      <button type="submit" id="settings-submit">Сменить пароль</button>
      <button type="button" id="settings-close">Закрыть</button>
    </form>
  </dialog>
</main>`;

function makeDeps(overrides: Partial<SettingsDeps> = {}): SettingsDeps {
  return {
    changePassword: vi.fn(async () => null),
    dataDir: vi.fn(async () => "C:\\Users\\daniil\\AppData\\Roaming\\pogrebok"),
    onInfo: vi.fn(),
    ...overrides,
  };
}

function settingsDialog(): HTMLDialogElement {
  const element = document.querySelector<HTMLDialogElement>("#settings-dialog");
  if (element === null) {
    throw new Error("нет #settings-dialog");
  }
  return element;
}

function fill(oldPw: string, newPw: string, new2: string): void {
  document.querySelector<HTMLInputElement>("#settings-old")!.value = oldPw;
  document.querySelector<HTMLInputElement>("#settings-new")!.value = newPw;
  document.querySelector<HTMLInputElement>("#settings-new2")!.value = new2;
}

function submitSettings(): void {
  document
    .querySelector<HTMLFormElement>("#settings-form")!
    .dispatchEvent(new Event("submit", { cancelable: true }));
}

async function until(check: () => void): Promise<void> {
  await vi.waitFor(check, { timeout: 1000, interval: 10 });
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("диалог настроек", () => {
  it("открывается и показывает папку данных", async () => {
    document.body.innerHTML = HTML;
    showSettings(makeDeps());
    expect(settingsDialog().open).toBe(true);
    await until(() => {
      expect(
        document.querySelector("#settings-data-dir")?.textContent,
      ).toContain("pogrebok");
    });
  });

  it("несовпадающие новые пароли — ошибка, мост не вызван", () => {
    document.body.innerHTML = HTML;
    const deps = makeDeps();
    showSettings(deps);
    fill("old", "new-1", "new-2");
    submitSettings();
    expect(
      document.querySelector("#settings-error")?.textContent,
    ).toContain("не совпадают");
    expect(deps.changePassword).not.toHaveBeenCalled();
    expect(settingsDialog().open).toBe(true);
  });

  it("пустой новый пароль — ошибка, мост не вызван", () => {
    document.body.innerHTML = HTML;
    const deps = makeDeps();
    showSettings(deps);
    fill("old", "", "");
    submitSettings();
    expect(
      document.querySelector("#settings-error")?.textContent,
    ).toContain("Укажите новый пароль");
    expect(deps.changePassword).not.toHaveBeenCalled();
  });

  it("неверный текущий пароль — ошибка под полем, диалог остаётся", async () => {
    document.body.innerHTML = HTML;
    const deps = makeDeps({
      changePassword: vi.fn(async () => "wrong_password"),
    });
    showSettings(deps);
    fill("bad", "new", "new");
    submitSettings();
    await until(() => {
      expect(
        document.querySelector("#settings-error")?.textContent,
      ).toContain("Неверный текущий пароль");
    });
    expect(deps.changePassword).toHaveBeenCalledWith("bad", "new");
    expect(settingsDialog().open).toBe(true);
    expect(deps.onInfo).not.toHaveBeenCalled();
  });

  it("успех: мост вызван, диалог закрыт, инфо-тост", async () => {
    document.body.innerHTML = HTML;
    const deps = makeDeps();
    showSettings(deps);
    fill("old", "new", "new");
    submitSettings();
    await until(() => {
      expect(deps.changePassword).toHaveBeenCalledWith("old", "new");
    });
    await until(() => {
      expect(settingsDialog().open).toBe(false);
    });
    expect(deps.onInfo).toHaveBeenCalledWith("Мастер-пароль изменён");
  });

  it("«Закрыть» и Esc (cancel) закрывают без смены пароля; повторное открытие чистое", () => {
    document.body.innerHTML = HTML;
    const deps = makeDeps();
    showSettings(deps);
    fill("old", "new", "new");
    document.querySelector<HTMLButtonElement>("#settings-close")!.click();
    expect(settingsDialog().open).toBe(false);
    expect(deps.changePassword).not.toHaveBeenCalled();

    showSettings(deps);
    expect(settingsDialog().open).toBe(true);
    // Поля очищены при повторном открытии.
    expect(
      document.querySelector<HTMLInputElement>("#settings-old")?.value,
    ).toBe("");
    settingsDialog().dispatchEvent(new Event("cancel", { cancelable: true }));
    expect(settingsDialog().open).toBe(false);
    expect(deps.changePassword).not.toHaveBeenCalled();
  });
});

describe("горячие клавиши", () => {
  function press(init: KeyboardEventInit): void {
    document.dispatchEvent(new KeyboardEvent("keydown", init));
  }

  it("Ctrl+, — настройки; Ctrl+L — диагностика (любая раскладка); F5 — обновить", () => {
    const handlers = {
      openSettings: vi.fn(),
      openDoctor: vi.fn(),
      refresh: vi.fn(),
    };
    const uninstall = installHotkeys(handlers);

    press({ key: ",", code: "Comma", ctrlKey: true });
    expect(handlers.openSettings).toHaveBeenCalledTimes(1);

    press({ key: "l", code: "KeyL", ctrlKey: true });
    expect(handlers.openDoctor).toHaveBeenCalledTimes(1);
    // Русская раскладка: та же физическая клавиша, другой event.key.
    press({ key: "д", code: "KeyL", ctrlKey: true });
    expect(handlers.openDoctor).toHaveBeenCalledTimes(2);

    press({ key: "F5", code: "F5" });
    expect(handlers.refresh).toHaveBeenCalledTimes(1);

    // Без Ctrl запятая — не хоткей; отписка гасит слушатель.
    press({ key: ",", code: "Comma" });
    expect(handlers.openSettings).toHaveBeenCalledTimes(1);
    uninstall();
    press({ key: "F5", code: "F5" });
    expect(handlers.refresh).toHaveBeenCalledTimes(1);
  });

  it("повторная установка подменяет обработчики, а не плодит слушателей", () => {
    const first = {
      openSettings: vi.fn(),
      openDoctor: vi.fn(),
      refresh: vi.fn(),
    };
    const second = {
      openSettings: vi.fn(),
      openDoctor: vi.fn(),
      refresh: vi.fn(),
    };
    const uninstall = installHotkeys(first);
    installHotkeys(second);
    press({ key: "F5", code: "F5" });
    expect(first.refresh).not.toHaveBeenCalled();
    expect(second.refresh).toHaveBeenCalledTimes(1);
    uninstall();
  });
});

describe("состояние окна", () => {
  function makeWindowDeps(store: Record<string, string>): WindowStateDeps {
    return {
      getSetting: vi.fn(async (key: string) => store[key] ?? null),
      setSetting: vi.fn(async (key: string, value: string) => {
        store[key] = value;
      }),
      applyWindowSize: vi.fn(async () => {}),
    };
  }

  it("восстановление: размер применяется, вкладка возвращается", async () => {
    const store = {
      [UI_WIN_W_KEY]: "1280",
      [UI_WIN_H_KEY]: "800",
      [UI_TAB_KEY]: "host",
    };
    const deps = makeWindowDeps(store);
    const tab = await restoreWindowState(deps);
    expect(deps.applyWindowSize).toHaveBeenCalledWith(1280, 800);
    expect(tab).toBe("host");
  });

  it("мусор и отсутствие значений игнорируются", async () => {
    const store = {
      [UI_WIN_W_KEY]: "abc",
      [UI_WIN_H_KEY]: "10",
      [UI_TAB_KEY]: "",
    };
    const deps = makeWindowDeps(store);
    expect(await restoreWindowState(deps)).toBeNull();
    expect(deps.applyWindowSize).not.toHaveBeenCalled();

    const empty = makeWindowDeps({});
    expect(await restoreWindowState(empty)).toBeNull();
    expect(empty.applyWindowSize).not.toHaveBeenCalled();
  });

  it("persistTab пишет ui.tab; resize — размеры с дебаунсом", async () => {
    const store: Record<string, string> = {};
    const deps = makeWindowDeps(store);
    persistTab(deps, "doctor");
    await until(() => {
      expect(store[UI_TAB_KEY]).toBe("doctor");
    });

    const uninstall = installWindowSizePersist(deps, 20);
    window.dispatchEvent(new Event("resize"));
    // Дебаунс ещё не отработал — записи нет.
    expect(store[UI_WIN_W_KEY]).toBeUndefined();
    await until(() => {
      expect(store[UI_WIN_W_KEY]).toBe(String(window.innerWidth));
    });
    expect(store[UI_WIN_H_KEY]).toBe(String(window.innerHeight));
    uninstall();
  });
});
