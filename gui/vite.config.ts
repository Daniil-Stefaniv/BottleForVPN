import { defineConfig } from "vitest/config";

// Источники фронтенда — в ui/ (план M4), тулчейн — в gui/package.json;
// strictPort, чтобы tauri dev всегда попадал на этот порт.
// outDir по умолчанию — dist относительно root → gui/ui/dist, откуда
// его забирает tauri.conf.json (frontendDist ../ui/dist от src-tauri).
export default defineConfig({
  root: "ui",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "es2022",
  },
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
});
