// Диалог «Логин и пароль» (п.9, спека §3.3): компактный модальный
// <dialog> поверх главного окна — логин, пароль, «Сохранить»/«Отмена».
// Значения уходят в зашифрованную БД; демон получает их только в память
// при старте бутылки. Вынесен из app.ts (Task 16), поведение прежнее.

/** Логин/пароль из диалога бутылки. */
export interface CredsAnswer {
  username: string;
  password: string;
}

/**
 * Показывает модальный диалог логина/пароля и ждёт ввода.
 * `null` — пользователь отменил. Пустой логин не принимается (ошибка
 * показывается в самом диалоге, ввод продолжается).
 */
export function askCredentials(id: string): Promise<CredsAnswer | null> {
  const dialog = document.querySelector<HTMLDialogElement>("#creds-dialog");
  const form = document.querySelector<HTMLFormElement>("#creds-form");
  const usernameInput = document.querySelector<HTMLInputElement>("#creds-username");
  const passwordInput = document.querySelector<HTMLInputElement>("#creds-password");
  const errorElement = document.querySelector<HTMLElement>("#creds-error");
  const idElement = document.querySelector<HTMLElement>("#creds-bottle-id");
  if (
    dialog === null ||
    form === null ||
    usernameInput === null ||
    passwordInput === null ||
    errorElement === null
  ) {
    return Promise.resolve(null);
  }
  if (idElement !== null) {
    idElement.textContent = id;
  }
  errorElement.hidden = true;
  errorElement.textContent = "";

  return new Promise((resolve) => {
    const cancelButton = document.querySelector<HTMLButtonElement>("#creds-cancel");

    const cleanup = (): void => {
      form.removeEventListener("submit", onSubmit);
      cancelButton?.removeEventListener("click", onCancel);
      dialog.removeEventListener("cancel", onCancel);
      dialog.close();
      form.reset();
    };
    const onCancel = (): void => {
      cleanup();
      resolve(null);
    };
    const onSubmit = (event: Event): void => {
      event.preventDefault();
      const username = usernameInput.value.trim();
      const password = passwordInput.value;
      if (username === "") {
        errorElement.textContent = "Укажите логин.";
        errorElement.hidden = false;
        usernameInput.focus();
        return;
      }
      cleanup();
      resolve({ username, password });
    };

    form.addEventListener("submit", onSubmit);
    cancelButton?.addEventListener("click", onCancel);
    // Esc в <dialog> генерирует cancel — та же отмена.
    dialog.addEventListener("cancel", onCancel);
    dialog.showModal();
    usernameInput.focus();
  });
}
