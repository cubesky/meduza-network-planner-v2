// Container health probe (no curl in the runtime image): exits 0 only if /readyz answers 200 within 4 s.
const port = process.env.PORT ?? "8787";
const controller = new AbortController();
const timer = setTimeout(() => controller.abort(), 4000);
fetch(`http://127.0.0.1:${port}/readyz`, { signal: controller.signal })
  .then((r) => process.exit(r.ok ? 0 : 1))
  .catch(() => process.exit(1))
  .finally(() => clearTimeout(timer));
