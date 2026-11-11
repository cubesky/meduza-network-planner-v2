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

// Output goes to dist/www, packaged as /usr/share/meduza-np/dashboard/www. Hashed file names under assets/ let the
// Rust server cache them forever while index.html is always revalidated (no stale UI after an upgrade).
export default defineConfig({
  root: __dirname,
  plugins: [themeBootstrap, preact()],
  build: {
    outDir: "../dist/www",
    emptyOutDir: true,
    sourcemap: false,
    target: "es2020",
    assetsDir: "assets",
    chunkSizeWarningLimit: 200,
  },
  server: {
    proxy: { "/api": "http://127.0.0.1:8443", "/auth": "http://127.0.0.1:8443" },
  },
});
