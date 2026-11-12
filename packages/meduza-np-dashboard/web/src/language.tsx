import { LOCALES, LOCALE_NAMES, browserLocale, setLocale, t, type Locale } from "@meduza-np/ui-kit";

const KEY = "mnp.locale";
function stored(): Locale | undefined {
  try {
    const value = localStorage.getItem(KEY) as Locale;
    return LOCALES.includes(value) ? value : undefined;
  } catch { return undefined; }
}
export const effectiveLocale = () => stored() ?? browserLocale();

/** Dashboard language is a browser preference; local login works without a Manager account. */
export function LanguageSelect() {
  return <select aria-label={t("Language")} value={stored() ?? ""} style={{ width: "auto" }} onChange={(event) => {
    const value = (event.target as HTMLSelectElement).value as Locale | "";
    try {
      if (value) localStorage.setItem(KEY, value);
      else localStorage.removeItem(KEY);
    } catch { /* The selection still applies to this document. */ }
    setLocale(value || browserLocale());
  }}>
    <option value="">{t("Browser language ({name})", { name: LOCALE_NAMES[browserLocale()] })}</option>
    {LOCALES.map((locale) => <option value={locale}>{LOCALE_NAMES[locale]}</option>)}
  </select>;
}
