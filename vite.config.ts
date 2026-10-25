import { defineConfig } from "vite";
// @ts-expect-error no @types/node in this project
import process from "node:process";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig(() => ({
  // Keep Rust errors visible in the terminal.
  clearScreen: false,
  server: {
    // Tauri loads the window from this fixed port.
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
