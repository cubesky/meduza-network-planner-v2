// Interface language: the account setting wins, then a choice made in this browser (e.g. on the sign-in page), then
// the browser's preferred languages. Choosing one while signed in stores it in the account settings.

import { LOCALES, LOCALE_NAMES, browserLocale, getLocale, setLocale, t, toast, type Locale } from "@meduza-np/ui-kit";

const KEY = "mnp.locale";

function storedLocale(): Locale | undefined {
  try {
    const v = localStorage.getItem(KEY);
    return LOCALES.includes(v as Locale) ? (v as Locale) : undefined;
  } catch {
    return undefined;
  }
}

function remember(l: Locale | null): void {
  try {
    if (l) localStorage.setItem(KEY, l);
    else localStorage.removeItem(KEY);
  } catch {
    // storage unavailable (private mode): the choice lasts for this page only
  }
}

/** Language to show for an account setting (undefined: not signed in or never chosen). */
export function effectiveLocale(account?: Locale): Locale {
  return account ?? storedLocale() ?? browserLocale();
}

/**
 * Language picker. `account` is the signed-in account's setting (`null` = follows the browser); with `onSave` the
 * choice is stored there, without it (sign-in page) only in this browser.
 */
export function LanguageSelect(props: { account?: Locale | null; onSave?: (l: Locale | null) => Promise<unknown> }) {
  const value = props.onSave ? (props.account ?? "") : (storedLocale() ?? "");
  return (
    <select
      aria-label={t("Language")}
      value={value}
      style={{ width: "auto" }}
      onChange={async (e) => {
        const v = (e.target as HTMLSelectElement).value;
        const l = v ? (v as Locale) : null;
        const previous = getLocale();
        remember(l);
        setLocale(l ?? browserLocale());
        if (!props.onSave) return;
        try {
          await props.onSave(l);
        } catch (err) {
          setLocale(previous);
          toast(t("Could not save the language: {error}", { error: err instanceof Error ? err.message : String(err) }), "bad");
        }
      }}
    >
      <option value="">{t("Browser language ({name})", { name: LOCALE_NAMES[browserLocale()] })}</option>
      {LOCALES.map((l) => <option value={l}>{LOCALE_NAMES[l]}</option>)}
    </select>
  );
}
