// Экран блокировки (0.3, пункт 7): без мастер-пароля GUI не пускает
// дальше. Три состояния: первый запуск (создание пароля с
// подтверждением и индикатором надёжности), разблокировка (ошибка под
// полем), «Забыли пароль?» — сброс с двойным подтверждением и честным
// текстом о потере данных. После успеха, если рядом найдены профили
// 0.2.x — предложение миграции одной кнопкой.
//
// API хранилища инжектируется (LockApi): прод — Tauri invoke из
// main-tauri.ts, тесты — заглушки.

/** Статус хранилища от команды `vault_status`. */
export interface LockStatus {
  status: "absent" | "locked";
  legacy_profiles: number;
}

/** Отчёт команды `legacy_import`. */
export interface ImportOutcome {
  imported: string[];
  skipped: [string, string][];
}

/** Мост к командам хранилища (в проде — Tauri invoke). */
export interface LockApi {
  createVault(password: string): Promise<void>;
  /** null — успех; "wrong_password" — неверный пароль; throw — беда. */
  unlockVault(password: string): Promise<string | null>;
  resetVault(): Promise<void>;
  legacyImport(): Promise<ImportOutcome>;
}

export interface LockOptions {
  status: LockStatus;
  api: LockApi;
  /** Вызывается один раз, когда хранилище разблокировано и GUI можно идти дальше. */
  onUnlocked: () => void;
}

/**
 * Оценка надёжности пароля 0..4: длина ≥8, длина ≥12, два класса
 * символов (нижний/верхний/цифры/прочие), три и более классов.
 */
export function scorePassword(pw: string): number {
  if (pw.length === 0) return 0;
  let score = 0;
  if (pw.length >= 8) score += 1;
  if (pw.length >= 12) score += 1;
  let classes = 0;
  if (/[a-zа-яё]/.test(pw)) classes += 1;
  if (/[A-ZА-ЯЁ]/.test(pw)) classes += 1;
  if (/\d/.test(pw)) classes += 1;
  if (/[^a-zа-яёA-ZА-ЯЁ\d]/.test(pw)) classes += 1;
  if (classes >= 2) score += 1;
  if (classes >= 3) score += 1;
  return score;
}

const METER_COLORS = ["#f87171", "#f87171", "#fbbf24", "#fbbf24", "#34d399"];

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** Рендерит экран блокировки в `root` (поверх главного окна). */
export function renderLock(root: HTMLElement, opts: LockOptions): void {
  const { api } = opts;

  root.innerHTML = "";
  root.hidden = false;
  const wrap = el("div", "lock");
  const card = el("div", "lock-card");
  wrap.append(card);
  root.append(wrap);

  const finish = () => {
    if (opts.status.legacy_profiles > 0) {
      renderMigration(card, root, opts);
    } else {
      root.hidden = true;
      opts.onUnlocked();
    }
  };

  if (opts.status.status === "absent") {
    renderCreate(card, api, finish);
  } else {
    renderUnlock(card, api, finish);
  }
}

/** Карточка: логотип, заголовок, подзаголовок. */
function head(card: HTMLElement, title: string, sub: string): void {
  card.innerHTML = "";
  card.append(el("div", "logo", "🍾"));
  const h = el("h4", undefined, title);
  h.className = "lock-title";
  card.append(h, el("div", "sub", sub));
}

/** Поле пароля с подписью-ошибкой под ним. */
function passwordField(
  placeholder: string,
): { input: HTMLInputElement; error: HTMLElement } {
  const input = el("input", "inp");
  input.type = "password";
  input.placeholder = placeholder;
  const error = el("div", "errmsg");
  error.hidden = true;
  return { input, error };
}

/** Состояние «разблокировка»: пароль, ошибка, ссылка «Забыли пароль?». */
function renderUnlock(
  card: HTMLElement,
  api: LockApi,
  finish: () => void,
): void {
  head(card, "Погребок", "хранилище зашифровано");
  const { input, error } = passwordField("мастер-пароль");
  const submit = el("button", "go-btn", "Разблокировать");
  submit.type = "button";
  const forgot = el("button", "link", "Забыли пароль? (сброс с потерей данных)");
  forgot.type = "button";
  card.append(input, error, submit, forgot);

  const tryUnlock = async () => {
    error.hidden = true;
    input.classList.remove("err");
    submit.setAttribute("disabled", "true");
    try {
      const result = await api.unlockVault(input.value);
      if (result === null) {
        finish();
      } else {
        error.textContent = "неверный пароль, попробуйте ещё раз";
        error.hidden = false;
        input.classList.add("err");
        input.value = "";
        input.focus();
      }
    } catch (e) {
      error.textContent = String(e);
      error.hidden = false;
      input.classList.add("err");
    } finally {
      submit.removeAttribute("disabled");
    }
  };
  submit.addEventListener("click", () => void tryUnlock());
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void tryUnlock();
  });
  forgot.addEventListener("click", () => renderForgotStep1(card, api, finish));
  input.focus();
}

/** Состояние «первый запуск»: создание пароля + подтверждение + метр. */
function renderCreate(
  card: HTMLElement,
  api: LockApi,
  finish: () => void,
): void {
  head(card, "Добро пожаловать", "первый запуск · придумайте мастер-пароль");
  const pass = passwordField("пароль");
  const meter = el("div", "meter");
  const meterBar = el("i");
  meter.append(meterBar);
  const repeat = passwordField("повторите пароль");
  const submit = el("button", "go-btn", "Создать хранилище");
  submit.type = "button";
  const note = el("span", "link", "восстановление невозможно — запомните его");
  card.append(pass.input, meter, pass.error, repeat.input, repeat.error, submit, note);

  pass.input.addEventListener("input", () => {
    const score = scorePassword(pass.input.value);
    meterBar.style.width = `${(score / 4) * 100}%`;
    meterBar.style.background = METER_COLORS[score];
  });

  const create = async () => {
    pass.error.hidden = true;
    repeat.error.hidden = true;
    repeat.input.classList.remove("err");
    if (pass.input.value.length < 8) {
      pass.error.textContent = "минимум 8 символов";
      pass.error.hidden = false;
      pass.input.classList.add("err");
      return;
    }
    if (pass.input.value !== repeat.input.value) {
      repeat.error.textContent = "пароли не совпадают";
      repeat.error.hidden = false;
      repeat.input.classList.add("err");
      return;
    }
    submit.setAttribute("disabled", "true");
    try {
      await api.createVault(pass.input.value);
      finish();
    } catch (e) {
      pass.error.textContent = String(e);
      pass.error.hidden = false;
    } finally {
      submit.removeAttribute("disabled");
    }
  };
  submit.addEventListener("click", () => void create());
  pass.input.focus();
}

/** «Забыли пароль?» шаг 1: честное предупреждение о потере данных. */
function renderForgotStep1(
  card: HTMLElement,
  api: LockApi,
  finish: () => void,
): void {
  head(card, "Сброс хранилища", "шаг 1 из 2");
  card.append(
    el(
      "p",
      "sub warn",
      "Сброс удалит все бутылки, конфиги и сохранённые логины/пароли безвозвратно. Демон и его процессы это не затронет.",
    ),
  );
  const next = el("button", "go-btn", "Продолжить");
  next.type = "button";
  const back = el("button", "link", "Назад, я вспомнил пароль");
  back.type = "button";
  card.append(next, back);
  next.addEventListener("click", () => renderForgotStep2(card, api, finish));
  back.addEventListener("click", () => renderUnlock(card, api, finish));
}

/** «Забыли пароль?» шаг 2: финальное подтверждение и сам сброс. */
function renderForgotStep2(
  card: HTMLElement,
  api: LockApi,
  finish: () => void,
): void {
  head(card, "Точно сбросить?", "шаг 2 из 2 · отменить будет нельзя");
  const confirm = el("button", "go-btn danger", "Сбросить хранилище");
  confirm.type = "button";
  const back = el("button", "link", "Отмена");
  back.type = "button";
  card.append(confirm, back);
  confirm.addEventListener("click", () => {
    void api.resetVault().then(() => renderCreate(card, api, finish));
  });
  back.addEventListener("click", () => renderUnlock(card, api, finish));
}

/** Предложение миграции профилей 0.2.x после разблокировки. */
function renderMigration(
  card: HTMLElement,
  root: HTMLElement,
  opts: LockOptions,
): void {
  head(
    card,
    "Найдены профили 0.2.x",
    `${opts.status.legacy_profiles} шт. · перенести в зашифрованное хранилище?`,
  );
  const report = el("p", "sub");
  report.hidden = true;
  const go = el("button", "go-btn", "Импортировать");
  go.type = "button";
  const skip = el("button", "link", "Пропустить (файлы останутся на диске)");
  skip.type = "button";
  card.append(report, go, skip);

  go.addEventListener("click", () => {
    go.setAttribute("disabled", "true");
    void opts.api
      .legacyImport()
      .then((outcome) => {
        const skipped = outcome.skipped.length;
        report.textContent =
          `Импортировано: ${outcome.imported.length}` +
          (skipped > 0 ? `, пропущено: ${skipped} (причины — в журнале)` : "");
        report.hidden = false;
        go.hidden = true;
        skip.textContent = "Продолжить";
      })
      .catch((e) => {
        report.textContent = String(e);
        report.hidden = false;
        go.removeAttribute("disabled");
      });
  });
  skip.addEventListener("click", () => {
    root.hidden = true;
    opts.onUnlocked();
  });
}
