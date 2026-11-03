import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
<<<<<<< HEAD
=======
  // Relative asset paths so the build works under /vault-ledger/ on GitHub Pages and inside Tauri.
  base: "./",
>>>>>>> c43bda1 (Update project files)
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
