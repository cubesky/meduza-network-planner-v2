// Interface languages for the manager and the node dashboard. Messages are keyed by their English text (English
// needs no dictionary); apps and shared libraries register their zh-Hans / zh-Hant dictionaries with addMessages.
// The active language is module state: the app root subscribes with useLocale() and re-renders the whole tree.

import { useEffect, useState } from "preact/hooks";

export const LOCALES = ["en", "zh-Hans", "zh-Hant"] as const;
export type Locale = (typeof LOCALES)[number];
export const LOCALE_NAMES: Record<Locale, string> = { en: "English", "zh-Hans": "简体中文", "zh-Hant": "繁體中文" };

type Dict = Record<string, string>;
const dicts: Record<Locale, Dict> = { en: {}, "zh-Hans": {}, "zh-Hant": {} };

export function addMessages(m: Partial<Record<Locale, Dict>>): void {
  for (const l of LOCALES) Object.assign(dicts[l], m[l]);
}

/** Maps a BCP 47 tag to a supported language: Traditional for zh-Hant/TW/HK/MO, Simplified for other zh. */
export function matchLocale(tag: string | null | undefined): Locale | undefined {
  if (!tag) return undefined;
  const t = tag.toLowerCase();
  if (!t.startsWith("zh")) return t.startsWith("en") ? "en" : undefined;
  if (/hant|-tw|-hk|-mo/.test(t)) return "zh-Hant";
  return "zh-Hans";
}

/** First supported language of the browser's preference list; English otherwise. */
export function browserLocale(langs: readonly string[] = typeof navigator === "undefined" ? [] : navigator.languages ?? [navigator.language]): Locale {
  for (const l of langs) {
    const m = matchLocale(l);
    if (m) return m;
  }
  return "en";
}

let current: Locale = browserLocale();
const listeners = new Set<(l: Locale) => void>();

export function getLocale(): Locale {
  return current;
}

export function setLocale(l: Locale): void {
  // always: the page's static <html lang> must follow even when the language does not change
  if (typeof document !== "undefined") document.documentElement.lang = l;
  if (l === current) return;
  current = l;
  for (const f of listeners) f(l);
}

/** Re-renders the calling component when the language changes (used once, at the app root). */
export function useLocale(): Locale {
  const [l, setL] = useState(current);
  useEffect(() => {
    listeners.add(setL);
    return () => void listeners.delete(setL);
  }, []);
  return l;
}

/** Translates `text` (the English message) and fills `{name}` placeholders. */
export function t(text: string, params?: Record<string, string | number>): string {
  const s = dicts[current][text] ?? text;
  return params ? s.replace(/\{(\w+)\}/g, (m, k: string) => (k in params ? String(params[k]) : m)) : s;
}

/** Marks a message defined outside rendering (constant tables) for extraction; translate it with t() when shown. */
export function msg(text: string): string {
  return text;
}
