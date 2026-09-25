import { defineConfig } from "vite";

// Dev server proxies API + avatars + WebSocket to the backend, so the SPA
// talks same-origin in development too. Run the backend on loopback HTTP:
//   VIRT_MAN_WEB_LOOPBACK_HTTP=1 VIRT_MAN_WEB_PORT=8787 cargo run -p virt-man-web
export default defineConfig({
  clearScreen: false,
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
