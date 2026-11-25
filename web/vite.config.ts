import path from "node:path";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  resolve: { alias: { "@": path.resolve(__dirname, "src") } },
  plugins: [react(), tailwindcss()],
  server: {
    // Parallel development across several sessions: PORT is assigned by the launcher (another port when 5173 is taken); when unset, the default is kept
    port: process.env.PORT ? Number(process.env.PORT) : 5173,
    proxy: {
      // The backend port can be overridden by UTOPIA_DEV_API (default 1516, matching UTOPIA_BIND_ADDR in .env)
      "/api": process.env.UTOPIA_DEV_API ?? "http://127.0.0.1:1516",
    },
  },
  test: {
    // Pure-logic unit tests with no DOM: the queryDefaults table and the merge of event invalidation
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
