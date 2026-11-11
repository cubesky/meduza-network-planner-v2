import { readFileSync } from "node:fs";
import { defineConfig, type Plugin } from "vite";
import preact from "@preact/preset-vite";

const themeSource = readFileSync(new URL("../../../web/ui-kit/src/theme-init.js", import.meta.url), "utf8");
const themeBootstrap: Plugin = {
  name: "meduza-theme-bootstrap",
  configureServer(server) {
    server.middlewares.use("/theme-init.js", (_req, res) => {
      res.setHeader("Content-Type", "text/javascript; charset=utf-8");
      res.end(themeSource);
    });
  },
  generateBundle() { this.emitFile({ type: "asset", fileName: "theme-init.js", source: themeSource }); },
  transformIndexHtml() { return [{ tag: "script", attrs: { src: "/theme-init.js" }, injectTo: "head-prepend" }]; },
};

// Production assets are served by the manager backend; the dev server proxies the API only for development.
export default defineConfig({
  root: __dirname,
  plugins: [themeBootstrap, preact()],
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
