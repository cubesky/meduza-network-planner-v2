// Blocking, same-origin bootstrap: restore the shared preference before CSS and the app can paint.
// Keep the storage key and system fallback aligned with theme.tsx. No inline script is needed under CSP.
(() => {
  let theme;
  try { theme = localStorage.getItem("mnp.theme"); } catch { /* Storage can be unavailable. */ }
  if (theme === "dark" || theme === "light") document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
})();
