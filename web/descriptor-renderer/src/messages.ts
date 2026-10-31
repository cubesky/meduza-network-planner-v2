// The renderer's own messages; package descriptors bring their texts in `i18n`.
import { addMessages } from "@meduza-np/ui-kit";
import zhHans from "./locales/zh-Hans.json";
import zhHant from "./locales/zh-Hant.json";

addMessages({ "zh-Hans": zhHans, "zh-Hant": zhHant });
