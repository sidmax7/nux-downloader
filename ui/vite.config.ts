import { defineConfig } from "vitest/config";
import vue from "@vitejs/plugin-vue";

// Tauri loads the dev server on a fixed port and the built `dist/` in
// release builds (see src-tauri/tauri.conf.json).
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "es2022", outDir: "dist" },
  test: { environment: "jsdom", include: ["src/**/*.test.ts"] },
});
