// Light / dark theme for the manager and the node dashboard. "system" follows prefers-color-scheme; an explicit
// choice sets data-theme on <html> (the tokens in styles.css) and is remembered in this browser.

import { useState } from "preact/hooks";
import { t } from "./i18n.ts";

export type Theme = "system" | "light" | "dark";
const KEY = "mnp.theme";

export function getTheme(): Theme {
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
  if (theme === "system") delete el.dataset.theme;
  else el.dataset.theme = theme;
}

export function setTheme(theme: Theme): void {
  try {
    if (theme === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, theme);
  } catch {
    // storage unavailable: the choice lasts for this page
  }
  applyTheme(theme);
}

export function ThemeSelect() {
  const [theme, set] = useState(getTheme());
  return (
    <select
      aria-label={t("Theme")}
      value={theme}
      style={{ width: "auto" }}
      onChange={(e) => {
        const v = (e.target as HTMLSelectElement).value as Theme;
        set(v);
        setTheme(v);
      }}
    >
      <option value="system">{t("Theme: follow system")}</option>
      <option value="light">{t("Theme: light")}</option>
      <option value="dark">{t("Theme: dark")}</option>
    </select>
  );
}
