// The manager's messages, keyed by their English text (tools: server/test/i18n.test.ts checks completeness).
import { addMessages } from "@meduza-np/ui-kit";
import zhHans from "./locales/zh-Hans.json";
import zhHant from "./locales/zh-Hant.json";

addMessages({ "zh-Hans": zhHans, "zh-Hant": zhHant });
