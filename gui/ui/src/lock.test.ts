// Тесты экрана блокировки (0.3, пункт 7): wrong_password не пускает
// дальше, первый запуск требует совпадения паролей, «Забыли пароль?» —
// два подтверждения до reset, предложение миграции 0.2.x.

import { describe, expect, it, vi } from "vitest";
import { renderLock, scorePassword } from "./lock";
import type { LockApi, LockStatus } from "./lock";

function makeApi(overrides: Partial<LockApi> = {}): LockApi {
  return {
    createVault: vi.fn(async () => {}),
    unlockVault: vi.fn(async () => null),
    resetVault: vi.fn(async () => {}),
    legacyImport: vi.fn(async () => ({ imported: [], skipped: [] })),
    ...overrides,
  };
}

function mount(
  status: LockStatus,
  api: LockApi,
): { root: HTMLElement; onUnlocked: ReturnType<typeof vi.fn> } {
  document.body.innerHTML = "";
  const root = document.createElement("div");
  document.body.append(root);
  const onUnlocked = vi.fn();
  renderLock(root, { status, api, onUnlocked });
  return { root, onUnlocked };
}

const click = (node: Element | null) =>
  (node as HTMLButtonElement | null)?.dispatchEvent(
    new MouseEvent("click", { bubbles: true }),
  );

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("экран блокировки", () => {
  it("показывает ошибку при wrong_password и не пропускает дальше", async () => {
    const api = makeApi({
      unlockVault: vi.fn(async () => "wrong_password"),
    });
    const { root, onUnlocked } = mount(
      { status: "locked", legacy_profiles: 0 },
      api,
    );

    const input = root.querySelector<HTMLInputElement>("input.inp")!;
    input.value = "неверный";
    click(root.querySelector("button.go-btn"));
    await flush();

    expect(onUnlocked).not.toHaveBeenCalled();
    const error = root.querySelector<HTMLElement>(".errmsg")!;
    expect(error.hidden).toBe(false);
    expect(error.textContent).toContain("неверный пароль");
    expect(input.classList.contains("err")).toBe(true);
  });

  it("верный пароль разблокирует и пропускает в приложение", async () => {
    const api = makeApi();
    const { root, onUnlocked } = mount(
      { status: "locked", legacy_profiles: 0 },
      api,
    );

    root.querySelector<HTMLInputElement>("input.inp")!.value = "верный-пароль";
    click(root.querySelector("button.go-btn"));
    await flush();

    expect(api.unlockVault).toHaveBeenCalledWith("верный-пароль");
    expect(onUnlocked).toHaveBeenCalledTimes(1);
    expect(root.hidden).toBe(true);
  });

  it("первый запуск требует совпадения паролей", async () => {
    const api = makeApi();
    const { root, onUnlocked } = mount(
      { status: "absent", legacy_profiles: 0 },
      api,
    );
    const [pass, repeat] = Array.from(
      root.querySelectorAll<HTMLInputElement>("input.inp"),
    );

    pass.value = "длинный-пароль-1";
    repeat.value = "другой-пароль-2";
    click(root.querySelector("button.go-btn"));
    await flush();

    expect(api.createVault).not.toHaveBeenCalled();
    expect(onUnlocked).not.toHaveBeenCalled();
    const errors = Array.from(
      root.querySelectorAll<HTMLElement>(".errmsg"),
    ).filter((e) => !e.hidden);
    expect(errors.some((e) => e.textContent?.includes("не совпадают"))).toBe(
      true,
    );

    // Совпали — хранилище создаётся, GUI идёт дальше.
    repeat.value = "длинный-пароль-1";
    click(root.querySelector("button.go-btn"));
    await flush();
    expect(api.createVault).toHaveBeenCalledWith("длинный-пароль-1");
    expect(onUnlocked).toHaveBeenCalledTimes(1);
  });

  it("первый запуск отклоняет пароль короче 8 символов", async () => {
    const api = makeApi();
    const { root } = mount({ status: "absent", legacy_profiles: 0 }, api);
    const [pass, repeat] = Array.from(
      root.querySelectorAll<HTMLInputElement>("input.inp"),
    );
    pass.value = "корот1";
    repeat.value = "корот1";
    click(root.querySelector("button.go-btn"));
    await flush();
    expect(api.createVault).not.toHaveBeenCalled();
    const errors = Array.from(
      root.querySelectorAll<HTMLElement>(".errmsg"),
    ).filter((e) => !e.hidden);
    expect(errors.some((e) => e.textContent?.includes("минимум 8"))).toBe(true);
  });

  it("забыли пароль требует двух подтверждений перед reset", async () => {
    const api = makeApi();
    const { root } = mount({ status: "locked", legacy_profiles: 0 }, api);

    click(
      Array.from(root.querySelectorAll("button.link")).find((b) =>
        b.textContent?.includes("Забыли пароль"),
      ) ?? null,
    );
    // Шаг 1: предупреждение, reset ещё не вызван.
    expect(root.textContent).toContain("Сброс удалит все бутылки");
    expect(api.resetVault).not.toHaveBeenCalled();

    click(
      Array.from(root.querySelectorAll("button.go-btn")).find((b) =>
        b.textContent?.includes("Продолжить"),
      ) ?? null,
    );
    // Шаг 2: финальное подтверждение, reset всё ещё не вызван.
    expect(root.textContent).toContain("Точно сбросить");
    expect(api.resetVault).not.toHaveBeenCalled();

    click(
      Array.from(root.querySelectorAll("button.go-btn")).find((b) =>
        b.textContent?.includes("Сбросить хранилище"),
      ) ?? null,
    );
    await flush();
    expect(api.resetVault).toHaveBeenCalledTimes(1);
    // После сброса — экран создания нового хранилища.
    expect(root.textContent).toContain("Добро пожаловать");
  });

  it("после разблокировки предлагает импорт профилей 0.2.x", async () => {
    const api = makeApi({
      legacyImport: vi.fn(async () => ({
        imported: ["office-a", "corp-host"],
        skipped: [["broken.toml", "битый профиль"]] as [string, string][],
      })),
    });
    const { root, onUnlocked } = mount(
      { status: "locked", legacy_profiles: 3 },
      api,
    );

    root.querySelector<HTMLInputElement>("input.inp")!.value = "пароль";
    click(root.querySelector("button.go-btn"));
    await flush();

    // Пока миграция не решена — onUnlocked не вызван.
    expect(onUnlocked).not.toHaveBeenCalled();
    expect(root.textContent).toContain("Найдены профили 0.2.x");

    click(
      Array.from(root.querySelectorAll("button.go-btn")).find((b) =>
        b.textContent?.includes("Импортировать"),
      ) ?? null,
    );
    await flush();
    expect(api.legacyImport).toHaveBeenCalledTimes(1);
    expect(root.textContent).toContain("Импортировано: 2");
    expect(root.textContent).toContain("пропущено: 1");

    click(
      Array.from(root.querySelectorAll("button.link")).find((b) =>
        b.textContent?.includes("Продолжить"),
      ) ?? null,
    );
    expect(onUnlocked).toHaveBeenCalledTimes(1);
    expect(root.hidden).toBe(true);
  });

  it("«Пропустить» миграцию пускает в приложение без импорта", async () => {
    const api = makeApi();
    const { root, onUnlocked } = mount(
      { status: "locked", legacy_profiles: 2 },
      api,
    );
    root.querySelector<HTMLInputElement>("input.inp")!.value = "пароль";
    click(root.querySelector("button.go-btn"));
    await flush();

    click(
      Array.from(root.querySelectorAll("button.link")).find((b) =>
        b.textContent?.includes("Пропустить"),
      ) ?? null,
    );
    expect(api.legacyImport).not.toHaveBeenCalled();
    expect(onUnlocked).toHaveBeenCalledTimes(1);
  });
});

describe("scorePassword", () => {
  it("оценивает длину и классы символов", () => {
    expect(scorePassword("")).toBe(0);
    expect(scorePassword("abc")).toBe(0); // короткий, один класс
    expect(scorePassword("abcdefgh")).toBe(1); // 8 символов, один класс
    expect(scorePassword("abcd1234")).toBe(2); // 8 символов, два класса
    expect(scorePassword("Abcd1234")).toBe(3); // 8 символов, три класса
    expect(scorePassword("Abcd1234!@#$")).toBe(4); // 12 символов, 4 класса
  });
});
