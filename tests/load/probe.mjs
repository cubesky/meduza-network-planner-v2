// Preloaded into the manager process by fleet.mjs (`node --import probe.mjs main.js`): reports the process' own
// RSS / heap / CPU over the IPC channel once per second. Does nothing when not spawned with IPC.

if (typeof process.send === "function") {
  process.channel?.unref();
  const timer = setInterval(() => {
    const m = process.memoryUsage();
    const c = process.cpuUsage();
    try {
      process.send({ probe: 1, at: Date.now(), rss: m.rss, heapUsed: m.heapUsed, external: m.external, cpuUs: c.user + c.system });
    } catch {
      // parent gone
    }
  }, 1000);
  timer.unref();
}
