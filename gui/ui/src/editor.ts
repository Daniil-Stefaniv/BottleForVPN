// Редактор бутылки (Task 16, спека §3.3): единая форма создания и
// редактирования. В Tauri живёт в дочернем окне ОС (editor.html,
// главное заблокировано), в демо — overlay поверх главного; разница —
// только в EditorHost. Переключатель env/host меняет набор полей; id
// генерируется из имени (редактируемо); чекбокс «сохранить логин и
// пароль» — только для OpenVPN и ведёт на второй шаг окна. Ошибки БД
// (дубль id и пр.) маппятся на поля формы (Review Focus #5).

import { gui } from "./backend";
import type {
  BottleEditorDataWire,
  EditorContextWire,
  SaveBottleArgsWire,
} from "./ipc";

/** Окружение окна редактора: контекст, выбор файла, закрытие. */
export interface EditorHost {
  context(): Promise<EditorContextWire>;
  pickConfig(): Promise<string | null>;
  close(): void;
}

let editorHost: EditorHost | null = null;

export function setEditorHost(host: EditorHost): void {
  editorHost = host;
}

function host(): EditorHost {
  if (editorHost === null) {
    throw new Error("окружение редактора не инициализировано (setEditorHost)");
  }
  return editorHost;
}

/** Подтверждение удаления; подменяется в тестах. */
let confirmDelete: (message: string) => boolean = (message) =>
  window.confirm(message);

export function setEditorConfirm(fn: (message: string) => boolean): void {
  confirmDelete = fn;
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

/** Транслитерация для автогенерации id из имени. */
const RU: Record<string, string> = {
  а: "a", б: "b", в: "v", г: "g", д: "d", е: "e", ё: "e", ж: "zh",
  з: "z", и: "i", й: "y", к: "k", л: "l", м: "m", н: "n", о: "o",
  п: "p", р: "r", с: "s", т: "t", у: "u", ф: "f", х: "h", ц: "c",
  ч: "ch", ш: "sh", щ: "sch", ъ: "", ы: "y", ь: "", э: "e", ю: "yu",
  я: "ya",
};

/**
 * Генерирует id из имени: транслит кириллицы, строчные, всё лишнее —
 * дефисы (схлопнутые, без краевых). «Офис Б» → «ofis-b».
 */
export function suggestId(name: string): string {
  const out: string[] = [];
  let dash = false;
  for (const ch of name.toLowerCase()) {
    const mapped = RU[ch] ?? (/[a-z0-9]/.test(ch) ? ch : "");
    if (mapped !== "") {
      if (dash && out.length > 0) {
        out.push("-");
      }
      dash = false;
      out.push(mapped);
    } else {
      dash = true;
    }
  }
  return out.join("");
}

/**
 * Маппинг ошибки сохранения на поле формы по ключевым словам.
 * Возвращает id поля (e-*) или null — покажем общей строкой.
 */
export function fieldForError(message: string): string | null {
  const lower = message.toLowerCase();
  if (lower.includes("уже существует") || lower.includes("идентификатор") || /\bid\b/.test(lower)) {
    return "e-id";
  }
  if (lower.includes("имя")) {
    return "e-name";
  }
  if (lower.includes("конфиг") || lower.includes("config")) {
    return "e-config";
  }
  if (lower.includes("vpn-клиент")) {
    return "e-vpn";
  }
  if (lower.includes("socks5") || lower.includes("порт")) {
    return "e-socks5";
  }
  if (lower.includes("dns")) {
    return "e-dns";
  }
  if (lower.includes("проброс") || lower.includes("forward")) {
    return "e-forwards";
  }
  return null;
}

/** Активные состояния: баннер «применится после перезапуска» + запрет удаления. */
const ACTIVE_STATES = new Set(["starting", "up", "degraded", "stopping"]);

interface EditorRefs {
  form: HTMLFormElement;
  title: HTMLElement;
  banner: HTMLElement;
  name: HTMLInputElement;
  id: HTMLInputElement;
  segEnv: HTMLButtonElement;
  segHost: HTMLButtonElement;
  sectionEnv: HTMLElement;
  sectionHost: HTMLElement;
  protocol: HTMLSelectElement;
  config: HTMLInputElement;
  configHint: HTMLElement;
  credsRow: HTMLElement;
  saveCreds: HTMLInputElement;
  vpn: HTMLInputElement;
  vpnArgs: HTMLInputElement;
  endpoints: HTMLInputElement;
  vpnInterface: HTMLInputElement;
  socks5: HTMLInputElement;
  dns: HTMLInputElement;
  forwards: HTMLInputElement;
  launch: HTMLInputElement;
  error: HTMLElement;
  deleteBtn: HTMLButtonElement;
  cancelBtn: HTMLButtonElement;
  submitBtn: HTMLButtonElement;
  stepCreds: HTMLElement;
  credsId: HTMLElement;
  username: HTMLInputElement;
  password: HTMLInputElement;
  credsError: HTMLElement;
  credsBack: HTMLButtonElement;
  credsSave: HTMLButtonElement;
}

function field(
  label: string,
  inputId: string,
  extra: string = "",
  span2 = false,
): string {
  return `<div class="field${span2 ? " span-2" : ""}">
    <label for="${inputId}">${label}</label>${extra}
  </div>`;
}

/** Разметка редактора (одна на окно ОС и на overlay в демо). */
function buildDom(root: HTMLElement): EditorRefs {
  root.innerHTML = `
  <form id="editor-form" class="ed">
    <div class="ed-scroll">
      <h2 id="e-title">Новая бутылка</h2>
      <p id="e-banner" class="ed-banner" hidden>
        Бутылка запущена — изменения применятся после её перезапуска.
      </p>
      <fieldset class="form-section">
        <legend>Основное</legend>
        <div class="form-grid">
          ${field("Имя", "e-name", `<input id="e-name" placeholder="Офис Б" autocomplete="off" />`)}
          ${field(
            "Идентификатор",
            "e-id",
            `<input id="e-id" placeholder="office-b" autocomplete="off" />
             <p class="field-hint">Уникальный, станет именем namespace в Linux.</p>
             <p class="field-error" data-for="e-id" hidden></p>`,
          )}
          <div class="field span-2">
            <label>Тип бутылки</label>
            <div class="seg" id="e-kind">
              <button type="button" class="seg-btn on" data-kind="env">env — окружение в Linux VM</button>
              <button type="button" class="seg-btn" data-kind="host">host — клиент на хосте</button>
            </div>
          </div>
        </div>
      </fieldset>
      <fieldset class="form-section" id="e-section-env">
        <legend>VPN</legend>
        <p class="section-hint">Туннель внутри изолированного окружения.</p>
        <div class="form-grid">
          ${field(
            "Протокол",
            "e-protocol",
            `<select id="e-protocol">
               <option value="wireguard">WireGuard</option>
               <option value="openvpn">OpenVPN</option>
             </select>`,
          )}
          ${field(
            "Файл конфигурации",
            "e-config",
            `<div class="input-row">
               <input id="e-config" placeholder="C:\\vpn\\wg-office.conf" />
               <button type="button" id="e-config-pick">Выбрать…</button>
             </div>
             <p class="field-hint" id="e-config-hint" hidden>
               Конфиг уже в хранилище — выберите файл, только чтобы заменить.
             </p>
             <p class="field-error" data-for="e-config" hidden></p>`,
          )}
          <div class="field span-2" id="e-creds-check-row" hidden>
            <label class="chk-row" for="e-save-creds">
              <input type="checkbox" id="e-save-creds" />
              сохранить логин и пароль в хранилище
            </label>
          </div>
        </div>
      </fieldset>
      <fieldset class="form-section" id="e-section-host" hidden>
        <legend>VPN</legend>
        <p class="section-hint">Фирменный клиент на хосте; маршруты ставит служба bottle-svc.</p>
        <div class="form-grid">
          ${field("VPN-клиент", "e-vpn", `<input id="e-vpn" placeholder="C:\\vpn\\anyconnect.exe" />`, true)}
          ${field("Аргументы клиента", "e-vpn-args", `<input id="e-vpn-args" />`)}
          ${field("Endpoint'ы (через пробел)", "e-endpoints", `<input id="e-endpoints" />`)}
          ${field("Интерфейс (необязательно)", "e-vpn-interface", `<input id="e-vpn-interface" />`, true)}
        </div>
      </fieldset>
      <fieldset class="form-section">
        <legend>Публикация</legend>
        <p class="section-hint">Что бутылка отдаёт наружу — только на 127.0.0.1.</p>
        <div class="form-grid">
          ${field(
            "Порт SOCKS5",
            "e-socks5",
            `<input id="e-socks5" type="number" value="0" min="0" max="65535" />
             <p class="field-hint">0 — автоподбор свободного порта.</p>
             <p class="field-error" data-for="e-socks5" hidden></p>`,
          )}
          ${field(
            "DNS внутри бутылки",
            "e-dns",
            `<input id="e-dns" placeholder="10.20.30.1" />
             <p class="field-error" data-for="e-dns" hidden></p>`,
          )}
          ${field(
            "TCP-пробросы (через пробел)",
            "e-forwards",
            `<input id="e-forwards" placeholder="10.20.30.15:3389" />
             <p class="field-hint">Формат host:port. Порт на 127.0.0.1 назначит демон.</p>
             <p class="field-error" data-for="e-forwards" hidden></p>`,
            true,
          )}
          ${field(
            "Команда после подъёма",
            "e-launch",
            `<input id="e-launch" placeholder="mstsc /v:127.0.0.1:13389" />
             <p class="field-hint">Запустится на хосте, когда туннель поднимется.</p>`,
            true,
          )}
        </div>
      </fieldset>
      <p id="e-error" class="ed-error" hidden></p>
    </div>
    <div class="ed-foot">
      <button type="button" id="e-delete" class="danger" hidden>Удалить бутылку</button>
      <span class="spacer"></span>
      <button type="button" id="e-cancel">Отмена</button>
      <button type="submit" id="e-submit" class="primary">Создать</button>
    </div>
  </form>
  <div id="e-step-creds" class="ed" hidden>
    <div class="ed-scroll">
      <h2>Логин и пароль</h2>
      <p class="section-hint">
        Бутылка «<span id="e-creds-id"></span>» · сохранятся в зашифрованной
        БД, демон получит их только в память при старте.
      </p>
      ${field("Логин", "e-username", `<input id="e-username" autocomplete="username" />`)}
      ${field("Пароль", "e-password", `<input id="e-password" type="password" autocomplete="new-password" />`)}
      <p id="e-creds-error" class="ed-error" hidden></p>
    </div>
    <div class="ed-foot">
      <button type="button" id="e-creds-back">Назад</button>
      <span class="spacer"></span>
      <button type="button" id="e-creds-save" class="primary">Сохранить</button>
    </div>
  </div>`;

  const q = <T extends HTMLElement>(selector: string): T => {
    const element = root.querySelector<T>(selector);
    if (element === null) {
      throw new Error(`разметка редактора: нет ${selector}`);
    }
    return element;
  };
  return {
    form: q<HTMLFormElement>("#editor-form"),
    title: q("#e-title"),
    banner: q("#e-banner"),
    name: q<HTMLInputElement>("#e-name"),
    id: q<HTMLInputElement>("#e-id"),
    segEnv: q<HTMLButtonElement>('[data-kind="env"]'),
    segHost: q<HTMLButtonElement>('[data-kind="host"]'),
    sectionEnv: q("#e-section-env"),
    sectionHost: q("#e-section-host"),
    protocol: q<HTMLSelectElement>("#e-protocol"),
    config: q<HTMLInputElement>("#e-config"),
    configHint: q("#e-config-hint"),
    credsRow: q("#e-creds-check-row"),
    saveCreds: q<HTMLInputElement>("#e-save-creds"),
    vpn: q<HTMLInputElement>("#e-vpn"),
    vpnArgs: q<HTMLInputElement>("#e-vpn-args"),
    endpoints: q<HTMLInputElement>("#e-endpoints"),
    vpnInterface: q<HTMLInputElement>("#e-vpn-interface"),
    socks5: q<HTMLInputElement>("#e-socks5"),
    dns: q<HTMLInputElement>("#e-dns"),
    forwards: q<HTMLInputElement>("#e-forwards"),
    launch: q<HTMLInputElement>("#e-launch"),
    error: q("#e-error"),
    deleteBtn: q<HTMLButtonElement>("#e-delete"),
    cancelBtn: q<HTMLButtonElement>("#e-cancel"),
    submitBtn: q<HTMLButtonElement>("#e-submit"),
    stepCreds: q("#e-step-creds"),
    credsId: q("#e-creds-id"),
    username: q<HTMLInputElement>("#e-username"),
    password: q<HTMLInputElement>("#e-password"),
    credsError: q("#e-creds-error"),
    credsBack: q<HTMLButtonElement>("#e-creds-back"),
    credsSave: q<HTMLButtonElement>("#e-creds-save"),
  };
}

/**
 * Монтирует редактор в `root` (body окна ОС или overlay-контейнер демо):
 * разметка, обработчики, загрузка контекста и профиля для edit.
 */
export async function mountEditor(root: HTMLElement): Promise<void> {
  const refs = buildDom(root);
  const context = await host().context();
  const isEdit = context.id !== null;
  const isActive = context.state !== null && ACTIVE_STATES.has(context.state);

  let kind: "env" | "host" = "env";
  let idTouched = isEdit; // в edit id readonly — автогенерации нет
  let hasStoredConfig = false;

  const syncKind = (): void => {
    refs.segEnv.classList.toggle("on", kind === "env");
    refs.segHost.classList.toggle("on", kind === "host");
    refs.sectionEnv.hidden = kind !== "env";
    refs.sectionHost.hidden = kind !== "host";
    syncCredsRow();
  };
  const syncCredsRow = (): void => {
    refs.credsRow.hidden =
      kind !== "env" || refs.protocol.value !== "openvpn";
  };

  refs.segEnv.addEventListener("click", () => {
    kind = "env";
    syncKind();
  });
  refs.segHost.addEventListener("click", () => {
    kind = "host";
    syncKind();
  });
  refs.protocol.addEventListener("change", syncCredsRow);
  refs.name.addEventListener("input", () => {
    if (!idTouched) {
      refs.id.value = suggestId(refs.name.value);
    }
  });
  refs.id.addEventListener("input", () => {
    idTouched = true;
  });
  root
    .querySelector<HTMLButtonElement>("#e-config-pick")
    ?.addEventListener("click", () => {
      void host()
        .pickConfig()
        .then((path) => {
          if (path !== null) {
            refs.config.value = path;
          }
        });
    });

  const clearErrors = (): void => {
    for (const error of root.querySelectorAll<HTMLElement>(".field-error")) {
      error.hidden = true;
      error.textContent = "";
    }
    for (const fieldEl of root.querySelectorAll<HTMLElement>("[aria-invalid]")) {
      fieldEl.removeAttribute("aria-invalid");
    }
    refs.error.hidden = true;
    refs.error.textContent = "";
    refs.credsError.hidden = true;
    refs.credsError.textContent = "";
  };

  const showFieldError = (fieldId: string, message: string): boolean => {
    const error = root.querySelector<HTMLElement>(`.field-error[data-for="${fieldId}"]`);
    const fieldEl = root.querySelector<HTMLElement>(`#${fieldId}`);
    if (error === null || fieldEl === null) {
      return false;
    }
    error.textContent = message;
    error.hidden = false;
    fieldEl.setAttribute("aria-invalid", "true");
    return true;
  };

  const readForm = (): SaveBottleArgsWire => {
    const args: SaveBottleArgsWire = {
      id: refs.id.value.trim(),
      name: refs.name.value.trim(),
      kind,
      is_edit: isEdit,
      socks5: Number.parseInt(refs.socks5.value, 10) || 0,
      forwards: refs.forwards.value.split(/[\s,]+/).filter((v) => v !== ""),
      save_credentials:
        kind === "env" &&
        refs.protocol.value === "openvpn" &&
        refs.saveCreds.checked,
    };
    if (kind === "env") {
      args.protocol = refs.protocol.value === "openvpn" ? "openvpn" : "wireguard";
      const config = refs.config.value.trim();
      if (config !== "") {
        args.config_path = config;
      }
    } else {
      args.client = refs.vpn.value.trim();
      if (refs.vpnArgs.value.trim() !== "") {
        args.args = refs.vpnArgs.value.trim();
      }
      args.endpoints = refs.endpoints.value.split(/[\s,]+/).filter((v) => v !== "");
      if (refs.vpnInterface.value.trim() !== "") {
        args.interface = refs.vpnInterface.value.trim();
      }
    }
    if (refs.dns.value.trim() !== "") {
      args.dns = refs.dns.value.trim();
    }
    if (refs.launch.value.trim() !== "") {
      args.launch = refs.launch.value.trim();
    }
    return args;
  };

  const showSaveError = (message: string, onCredsStep: boolean): void => {
    const fieldId = fieldForError(message);
    if (fieldId !== null && showFieldError(fieldId, message)) {
      if (onCredsStep) {
        // Поле на первом шаге — вернуться к нему.
        refs.stepCreds.hidden = true;
        refs.form.hidden = false;
      }
      return;
    }
    const target = onCredsStep ? refs.credsError : refs.error;
    target.textContent = message;
    target.hidden = false;
  };

  const doSave = async (args: SaveBottleArgsWire, onCredsStep: boolean): Promise<void> => {
    try {
      await gui().saveBottle(args);
      host().close();
    } catch (error) {
      showSaveError(error instanceof Error ? error.message : String(error), onCredsStep);
    }
  };

  refs.form.addEventListener("submit", (event) => {
    event.preventDefault();
    clearErrors();
    const args = readForm();
    if (!isEdit) {
      const idError = validateBottleId(args.id);
      if (idError !== null) {
        showFieldError("e-id", idError);
        return;
      }
    }
    if (args.name === "") {
      showFieldError("e-id", "");
      refs.error.textContent = "Укажите имя бутылки.";
      refs.error.hidden = false;
      return;
    }
    if (
      kind === "env" &&
      args.config_path === undefined &&
      !hasStoredConfig
    ) {
      showFieldError("e-config", "Укажите файл конфигурации туннеля.");
      return;
    }
    if (args.save_credentials === true) {
      // Второй шаг: логин/пароль для хранилища.
      refs.credsId.textContent = args.id;
      refs.form.hidden = true;
      refs.stepCreds.hidden = false;
      refs.username.focus();
      return;
    }
    void doSave(args, false);
  });

  refs.credsBack.addEventListener("click", () => {
    refs.stepCreds.hidden = true;
    refs.form.hidden = false;
  });
  refs.credsSave.addEventListener("click", () => {
    refs.credsError.hidden = true;
    const username = refs.username.value.trim();
    if (username === "") {
      refs.credsError.textContent = "Укажите логин.";
      refs.credsError.hidden = false;
      refs.username.focus();
      return;
    }
    const args = readForm();
    args.username = username;
    args.password = refs.password.value;
    void doSave(args, true);
  });

  refs.cancelBtn.addEventListener("click", () => {
    host().close();
  });
  refs.deleteBtn.addEventListener("click", () => {
    const id = context.id;
    if (id === null) {
      return;
    }
    if (
      !confirmDelete(
        `Удалить бутылку «${id}»? Профиль, конфиг и сохранённые ` +
          "логин/пароль будут стёрты из хранилища без возможности восстановления.",
      )
    ) {
      return;
    }
    void gui()
      .deleteBottle(id)
      .then(() => {
        host().close();
      })
      .catch((error: unknown) => {
        refs.error.textContent =
          error instanceof Error ? error.message : String(error);
        refs.error.hidden = false;
      });
  });

  // Режим edit: профиль в форму, id readonly, удаление и баннер.
  if (isEdit && context.id !== null) {
    const data = await gui().getBottle(context.id);
    if (data === null) {
      refs.error.textContent = `Профиль бутылки не найден: ${context.id}`;
      refs.error.hidden = false;
      refs.submitBtn.disabled = true;
      return;
    }
    fillForm(refs, data, (nextKind) => {
      kind = nextKind;
      syncKind();
    });
    hasStoredConfig = data.has_config;
    refs.configHint.hidden = !data.has_config;
    if (data.has_config && "protocol" in data.profile.vpn) {
      refs.config.placeholder = data.profile.vpn.config;
    }
    refs.id.readOnly = true;
    refs.title.textContent = `Изменить «${data.profile.name}»`;
    refs.submitBtn.textContent = "Сохранить";
    refs.deleteBtn.hidden = false;
    if (isActive) {
      refs.banner.hidden = false;
      refs.deleteBtn.disabled = true;
      refs.deleteBtn.title = "Сначала остановите бутылку";
    }
  }
  syncKind();
}

/** Заполняет форму профилем из БД (edit). */
function fillForm(
  refs: EditorRefs,
  data: BottleEditorDataWire,
  setKind: (kind: "env" | "host") => void,
): void {
  const profile = data.profile;
  refs.id.value = profile.id;
  refs.name.value = profile.name;
  refs.socks5.value = String(profile.expose.socks5);
  refs.forwards.value = profile.expose.forwards.map((f) => f.target).join(" ");
  refs.dns.value = profile.expose.dns ?? "";
  refs.launch.value = profile.expose.launch ?? "";
  if (profile.kind === "env" && "protocol" in profile.vpn) {
    setKind("env");
    refs.protocol.value = profile.vpn.protocol;
    // Сам конфиг — в БД; поле файла пустое (подстановка — placeholder).
  } else if (profile.kind === "host" && "client" in profile.vpn) {
    setKind("host");
    refs.vpn.value = profile.vpn.client;
    refs.vpnArgs.value = profile.vpn.args ?? "";
    refs.endpoints.value = profile.vpn.endpoints.join(" ");
    refs.vpnInterface.value = profile.vpn.interface ?? "";
  }
}
