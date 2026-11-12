// Light / dark theme for the manager and the node dashboard. "system" follows prefers-color-scheme; an explicit
// choice sets data-theme on <html> (the tokens in styles.css) and is remembered in this browser.

import { useEffect, useState } from "preact/hooks";
import { t } from "./i18n.ts";

export type Theme = "system" | "light" | "dark";
const KEY = "mnp.theme";
let temporary: Theme | undefined;
const listeners = new Set<(theme: Theme) => void>();
let started = false;

export function getTheme(): Theme {
  if (temporary) return temporary;
  try {
    const v = localStorage.getItem(KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

export function applyTheme(theme: Theme = getTheme()): void {
  if (typeof document === "undefined") return;
  const el = document.documentElement;
  if (theme === "system") {
    if (el.dataset.theme !== undefined) delete el.dataset.theme;
  } else if (el.dataset.theme !== theme) el.dataset.theme = theme;
}

/** The stored choice owns the document theme, including after DOM restoration and cross-tab changes. */
export function initializeTheme(): void {
  const sync = () => {
    const theme = getTheme();
    applyTheme(theme);
    for (const listener of listeners) listener(theme);
  };
  sync();
  if (started || typeof window === "undefined" || typeof document === "undefined") return;
  started = true;
  new MutationObserver(sync).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  window.addEventListener("pageshow", sync);
  window.addEventListener("storage", (event) => {
    if (event.key === KEY || event.key === null) { temporary = undefined; sync(); }
  });
}

export function setTheme(theme: Theme): void {
  temporary = undefined;
  try {
    if (theme === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, theme);
  } catch {
    // storage unavailable: the choice lasts for this page
    temporary = theme;
  }
  applyTheme(theme);
  for (const listener of listeners) listener(theme);
}

export function ThemeSelect() {
  const [theme, set] = useState(getTheme());
  useEffect(() => {
    listeners.add(set);
    initializeTheme();
    return () => { listeners.delete(set); };
  }, []);
  return (
    <select
      aria-label={t("Theme")}
      value={theme}
      style={{ width: "auto" }}
      onChange={(e) => {
        const v = (e.target as HTMLSelectElement).value as Theme;
        setTheme(v);
      }}
    >
      <option value="system">{t("Theme: follow system")}</option>
      <option value="light">{t("Theme: light")}</option>
      <option value="dark">{t("Theme: dark")}</option>
    </select>
  );
}
