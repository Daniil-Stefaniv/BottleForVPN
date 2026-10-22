import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// Источники фронтенда — в ui/ (план M4), тулчейн — в gui/package.json;
// strictPort, чтобы tauri dev всегда попадал на этот порт.
// outDir по умолчанию — dist относительно root → gui/ui/dist, откуда
// его забирает tauri.conf.json (frontendDist ../ui/dist от src-tauri).
// Входы rollup — абсолютные: относительные резолвятся от cwd, а не root.
const ui = (path: string): string =>
  fileURLToPath(new URL(`./ui/${path}`, import.meta.url));

export default defineConfig({
  root: "ui",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "es2022",
    rollupOptions: {
      input: {
        main: ui("index.html"),
        editor: ui("editor.html"),
      },
    },
  },
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
});
