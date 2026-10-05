// Точка входа GUI (M4 Task 2): каркас проверяет доступность CLI
// через команду-посредник run_bottle. Экраны списка/создания/doctor
// подключаются в последующих задачах (Task 3–5).

import { invoke } from "@tauri-apps/api/core";

/** Ответ команды-посредника: stdout подпроцесса `bottle` и его код. */
export interface BottleOutput {
  stdout: string;
  code: number;
}

/**
 * Выполняет `bottle <args>` как сайдкар-подпроцесс (рулинг D1):
 * вся бизнес-логика — в CLI, GUI только рендерит.
 */
export async function runBottle(args: string[]): Promise<BottleOutput> {
  return await invoke<BottleOutput>("run_bottle", { args });
}

async function init(): Promise<void> {
  const element = document.querySelector<HTMLParagraphElement>("#bottle-version");
  if (element === null) {
    return;
  }
  try {
    const output = await runBottle(["--version", "--json"]);
    element.textContent = `CLI: ${output.stdout.trim()}`;
  } catch (error) {
    element.textContent = `CLI недоступен: ${error}`;
  }
}

void init();
