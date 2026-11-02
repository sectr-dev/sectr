import { defineCollection } from "astro:content";
import { docsLoader, i18nLoader } from "@astrojs/starlight/loaders";
import { docsSchema, i18nSchema } from "@astrojs/starlight/schema";

// The i18n collection must be defined and non-empty even on this
// monolingual site: Starlight calls getCollection("i18n") on startup, and
// Astro warns "The collection \"i18n\" does not exist or is empty" unless
// the loader produced at least one entry (src/content/i18n/en.json).
export const collections = {
    docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
    i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};