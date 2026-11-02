import { defineConfig } from "vite";
import { configDefaults } from "vitest/config";
import react from "@vitejs/plugin-react";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },

  test: {
    // `scripts/plugins/probe-report.test.mjs` uses `node:test`, not vitest, so
    // the probe stays runnable with plain
    // `node --test desktop/scripts/plugins/probe-report.test.mjs` (the path the
    // plan specifies) while `npm test` stays green. Vitest's default include
    // would otherwise collect it and fail with "No test suite found".
    exclude: [...configDefaults.exclude, "scripts/plugins/**"],
  },
}));
