import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

// Production assets are served by the manager backend; the dev server proxies the API only for development.
export default defineConfig({
  root: __dirname,
  plugins: [preact()],
  build: {
    outDir: "../dist/web",
    emptyOutDir: true,
    sourcemap: false,
    target: "es2020",
    chunkSizeWarningLimit: 400,
  },
  server: {
    proxy: { "/api": { target: "http://127.0.0.1:8787", ws: true } },
  },
});
