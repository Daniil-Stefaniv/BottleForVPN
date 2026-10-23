// Настройки главного окна (Task 18, спека §3.1/§9): модальный диалог со
// сменой мастер-пароля (старый + новый×2 → PRAGMA rekey на Rust-стороне)
// и путём к папке данных; горячие клавиши (Ctrl+, настройки, Ctrl+L
// Диагностика, F5 обновить — по event.code, чтобы работали в любой
// раскладке; Esc закрывает модальное штатным cancel у <dialog>);
// состояние окна (размер, активная вкладка) в meta-ключах ui.* зашифрованной
// БД с восстановлением при запуске.

/** Ключи meta-хранилища для состояния окна. */
export const UI_TAB_KEY = "ui.tab";
export const UI_WIN_W_KEY = "ui.win_w";
export const UI_WIN_H_KEY = "ui.win_h";

/** Зависимости диалога настроек (собирает app.ts из GuiApi). */
export interface SettingsDeps {
  /** null — пароль изменён; "wrong_password" — неверный текущий. */
  changePassword(oldPassword: string, newPassword: string): Promise<string | null>;
  /** Папка данных приложения (показывается в диалоге). */
  dataDir(): Promise<string>;
  /** Краткое инфо-сообщение (тост успеха). */
  onInfo?(message: string): void;
}

/**
 * Открывает модальный диалог настроек. Повторный вызов при открытом
 * диалоге — no-op. Поля паролей очищаются при каждом открытии.
 */
export function showSettings(deps: SettingsDeps): void {
  const dialog = document.querySelector<HTMLDialogElement>("#settings-dialog");
  const form = document.querySelector<HTMLFormElement>("#settings-form");
  const oldInput = document.querySelector<HTMLInputElement>("#settings-old");
  const newInput = document.querySelector<HTMLInputElement>("#settings-new");
  const new2Input = document.querySelector<HTMLInputElement>("#settings-new2");
  const errorElement = document.querySelector<HTMLElement>("#settings-error");
  const dataDirElement = document.querySelector<HTMLElement>("#settings-data-dir");
  const closeButton = document.querySelector<HTMLButtonElement>("#settings-close");
  if (
    dialog === null ||
    form === null ||
    oldInput === null ||
    newInput === null ||
    new2Input === null ||
    errorElement === null ||
    closeButton === null
  ) {
    return;
  }
  if (dialog.open) {
    return;
  }

  const showError = (message: string): void => {
    errorElement.textContent = message;
    errorElement.hidden = false;
  };

  oldInput.value = "";
  newInput.value = "";
  new2Input.value = "";
  errorElement.hidden = true;
  errorElement.textContent = "";
  if (dataDirElement !== null) {
    dataDirElement.textContent = "…";
    deps
      .dataDir()
      .then((dir) => {
        dataDirElement.textContent = dir;
      })
      .catch(() => {
        dataDirElement.textContent = "недоступна";
      });
  }

  const cleanup = (): void => {
    form.removeEventListener("submit", onSubmit);
    closeButton.removeEventListener("click", onClose);
    dialog.removeEventListener("cancel", onCancel);
  };
  const onClose = (): void => {
    cleanup();
    dialog.close();
  };
  // Esc у <dialog> генерирует cancel — то же закрытие без изменений.
  const onCancel = (): void => {
    cleanup();
    dialog.close();
  };
  const onSubmit = (event: Event): void => {
    event.preventDefault();
    const oldPassword = oldInput.value;
    const newPassword = newInput.value;
    if (newPassword === "") {
      showError("Укажите новый пароль.");
      newInput.focus();
      return;
    }
    if (newPassword !== new2Input.value) {
      showError("Новые пароли не совпадают.");
      new2Input.focus();
      return;
    }
    errorElement.hidden = true;
    deps
      .changePassword(oldPassword, newPassword)
      .then((result) => {
        if (result === "wrong_password") {
          showError("Неверный текущий пароль.");
          oldInput.focus();
          return;
        }
        cleanup();
        dialog.close();
        deps.onInfo?.("Мастер-пароль изменён");
      })
      .catch((error: unknown) => {
        showError(error instanceof Error ? error.message : String(error));
      });
  };

  form.addEventListener("submit", onSubmit);
  closeButton.addEventListener("click", onClose);
  dialog.addEventListener("cancel", onCancel);
  dialog.showModal();
  oldInput.focus();
}

// ---------------------------------------------------------------------------
// Горячие клавиши
// ---------------------------------------------------------------------------

export interface HotkeyHandlers {
  /** Ctrl+, — открыть настройки. */
  openSettings(): void;
  /** Ctrl+L — вкладка Диагностики. */
  openDoctor(): void;
  /** F5 — обновить список и статусы. */
  refresh(): void;
}

/**
 * Единственный слушатель на документе: при первом вызове ставится, дальше
 * installHotkeys лишь подменяет текущие обработчики — перезагрузка модулей
 * в тестах и повторный mountApp не плодят дубликаты. Возвращает отписку
 * (снимает слушатель, если это была первая установка).
 */
export function installHotkeys(handlers: HotkeyHandlers): () => void {
  const marker = "__pogrebokHotkeys";
  const host = document as unknown as Record<string, unknown>;
  const swap = host[marker] as ((next: HotkeyHandlers) => void) | undefined;
  if (swap !== undefined) {
    swap(handlers);
    return () => {};
  }

  let current = handlers;
  const onKeyDown = (event: KeyboardEvent): void => {
    if (event.ctrlKey && !event.altKey && !event.shiftKey) {
      if (event.code === "Comma" || event.key === ",") {
        event.preventDefault();
        current.openSettings();
        return;
      }
      if (event.code === "KeyL") {
        event.preventDefault();
        current.openDoctor();
        return;
      }
    }
    if (event.code === "F5" || event.key === "F5") {
      // В webview F5 по умолчанию перезагружает страницу — перехватываем.
      event.preventDefault();
      current.refresh();
    }
    // Esc не перехватываем: модальные <dialog> закрываются штатным cancel.
  };
  document.addEventListener("keydown", onKeyDown);
  host[marker] = (next: HotkeyHandlers): void => {
    current = next;
  };
  return () => {
    document.removeEventListener("keydown", onKeyDown);
    delete host[marker];
  };
}

// ---------------------------------------------------------------------------
// Состояние окна (ui.*)
// ---------------------------------------------------------------------------

export interface WindowStateDeps {
  getSetting(key: string): Promise<string | null>;
  setSetting(key: string, value: string): Promise<void>;
  applyWindowSize(width: number, height: number): Promise<void>;
}

/**
 * Восстанавливает состояние окна из БД: применяет сохранённый размер
 * (мусор/отсутствие игнорируются) и возвращает сохранённую вкладку
 * (null — открыть вкладку по умолчанию).
 */
export async function restoreWindowState(
  deps: WindowStateDeps,
): Promise<string | null> {
  const [widthRaw, heightRaw, tab] = await Promise.all([
    deps.getSetting(UI_WIN_W_KEY),
    deps.getSetting(UI_WIN_H_KEY),
    deps.getSetting(UI_TAB_KEY),
  ]);
  const width = Number(widthRaw);
  const height = Number(heightRaw);
  if (
    Number.isFinite(width) &&
    Number.isFinite(height) &&
    width >= 400 &&
    width <= 10_000 &&
    height >= 300 &&
    height <= 10_000
  ) {
    await deps.applyWindowSize(width, height);
  }
  return tab !== null && tab !== "" ? tab : null;
}

/** Сохраняет активную вкладку (fire-and-forget: БД не должна ломать UI). */
export function persistTab(
  deps: Pick<WindowStateDeps, "setSetting">,
  tab: string,
): void {
  void deps.setSetting(UI_TAB_KEY, tab).catch(() => undefined);
}

/**
 * Сохраняет размер окна на resize с дебаунсом. Возвращает отписку
 * (снимает слушатель и отменяет отложенную запись).
 */
export function installWindowSizePersist(
  deps: Pick<WindowStateDeps, "setSetting">,
  debounceMs = 500,
): () => void {
  let timer = 0;
  const onResize = (): void => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      void deps
        .setSetting(UI_WIN_W_KEY, String(window.innerWidth))
        .catch(() => undefined);
      void deps
        .setSetting(UI_WIN_H_KEY, String(window.innerHeight))
        .catch(() => undefined);
    }, debounceMs);
  };
  window.addEventListener("resize", onResize);
  return () => {
    window.removeEventListener("resize", onResize);
    window.clearTimeout(timer);
  };
}
