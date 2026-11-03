import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  build: {
    // The app CSP only allows fonts from 'self', so never inline them as data: URIs.
    assetsInlineLimit: 0,
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
