// Маршрутизатор точки входа GUI:
//  - `index.html?demo=1`    — демо-режим без демона и Tauri (разработка
//    UI в обычном браузере, скриншоты); данные из ./demo;
//  - `index.html?demo=down` — то же, но с панелью «демон не запущен»;
//  - без параметров         — боевой запуск под Tauri (./main-tauri).
// Tauri-модули подключаются динамически, чтобы в браузере не грузился
// код, завязанный на window.__TAURI_INTERNALS__.

export {};

const params = new URLSearchParams(window.location.search);
const demo = params.get("demo");

if (demo !== null) {
  const { mountDemo } = await import("./demo");
  mountDemo(demo === "down", params.get("view"));
} else {
  await import("./main-tauri");
}
