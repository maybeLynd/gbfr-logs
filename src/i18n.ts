import i18n from "i18next";
import LanguageDetector from "i18next-browser-languagedetector";
import resourcesToBackend from "i18next-resources-to-backend";
import { initReactI18next } from "react-i18next";

import { readTextFile } from "@tauri-apps/api/fs";
import { resolveResource } from "@tauri-apps/api/path";

const RESOURCE_LANGUAGE_ALIASES: Record<string, string> = {
  "ko-KR": "ko",
  "fr-FR": "fr",
  "es-ES": "es",
  "it-IT": "it",
};

const loadLanguageFromPath = async (language: string, namespace: string) => {
  const resourceLanguage = RESOURCE_LANGUAGE_ALIASES[language] ?? language;
  const resourcePath = await resolveResource(`lang/${resourceLanguage}/${namespace}.json`);
  return JSON.parse(await readTextFile(resourcePath));
};

export const SUPPORTED_LANGUAGES: { [key: string]: string } = {
  en: "English",
  "zh-CN": "简体中文",
  "zh-TW": "繁體中文",
  "ko-KR": "한국어",
  jp: "日本語",
  "fr-FR": "Français",
  bp: "Brazillian Portuguese",
  ge: "Deutsch",
  "es-ES": "Español",
  "it-IT": "Italiano",
};

i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .use(
    resourcesToBackend((language, namespace, callback) => {
      loadLanguageFromPath(language, namespace)
        .then((res) => callback(null, res))
        .catch((error) => callback(error, null));
    })
  )
  .init({
    ns: ["ui", "characters", "items", "overmasteries", "sigils", "traits", "weapons", "quests", "enemies", "locations"],
    defaultNS: "ui",
    fallbackLng: {
      default: ["en"],
      "zh-TW": ["zh-CN", "en"],
    },
    interpolation: {
      escapeValue: false,
    },
    react: {
      bindI18n: "languageChanged loaded",
      bindI18nStore: "added",
    },
  });

declare global {
  interface Window {
    /* eslint-disable */
    i18n: any;
  }
}

window.i18n = i18n;

export default i18n;
