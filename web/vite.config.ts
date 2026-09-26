import { defineConfig } from "vite";
import process from "node:process";

// VM_IIFE builds the same entry as a classic (non-module) script so the DOM
// smoke test can execute it: jsdom silently skips <script type="module">.
const iife = process.env.VM_IIFE === "1";

// Dev server proxies API + avatars + WebSocket to the backend, so the SPA
// talks same-origin in development too. Run the backend on loopback HTTP:
//   VIRT_MAN_WEB_LOOPBACK_HTTP=1 VIRT_MAN_WEB_PORT=8787 cargo run -p virt-man-web
export default defineConfig({
  clearScreen: false,
  build: iife
    ? {
        lib: { entry: "src/main.ts", name: "VirtMan", formats: ["iife"], fileName: () => "app.iife.js" },
        emptyOutDir: true,
        // The console libraries are loaded on demand and carry top-level
        // await, which no classic script can express. The DOM smoke test
        // never opens a console, so leave them unbundled.
        rollupOptions: { external: ["@novnc/novnc", "@spice-project/spice-html5/src/main.js"] },
      }
    : {},
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/proxy": {
        target: "ws://127.0.0.1:8787",
        ws: true,
      },
      "/auth": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/connections": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/users": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/me": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/keys": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/avatars": { target: "http://127.0.0.1:8787", changeOrigin: true },
    },
  },
});
