import { defineCollection } from 'astro:content';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

export const collections = {
	docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
	// Declared so Starlight's translation lookup does not warn that `i18n` is missing.
	i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};
