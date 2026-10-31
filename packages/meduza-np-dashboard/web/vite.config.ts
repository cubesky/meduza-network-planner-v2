import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

// Output goes to dist/www, packaged as /usr/share/meduza-np/dashboard/www. Hashed file names under assets/ let the
// Rust server cache them forever while index.html is always revalidated (no stale UI after an upgrade).
export default defineConfig({
  root: __dirname,
  plugins: [preact()],
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
