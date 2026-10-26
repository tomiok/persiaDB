import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { breadcrumbJsonLd, jsonLdScript } from './breadcrumbs';
import { PROGRESS_TOKEN, progressSentence } from './progress.mjs';

// Frontmatter `head` (the FAQ JSON-LD) is not Markdown, so the progress plugin
// never sees it. Substitute the same sentence the pages use.
function fillProgress(value: string): string {
	if (!value.includes(PROGRESS_TOKEN)) return value;
	return value.replaceAll(PROGRESS_TOKEN, progressSentence());
}

function fillHead(head: { content?: unknown; attrs?: Record<string, unknown> }[]): void {
	for (const entry of head) {
		if (typeof entry.content === 'string') entry.content = fillProgress(entry.content);
		const attrs = entry.attrs;
		if (!attrs) continue;
		for (const key of Object.keys(attrs)) {
			const value = attrs[key];
			if (typeof value === 'string') attrs[key] = fillProgress(value);
		}
	}
}

// Appended to the route `head` array Starlight already built from config and
// frontmatter, so the homepage SoftwareSourceCode JSON-LD, the FAQ JSON-LD,
// Open Graph tags, and the 404 noindex stay in place.
export const onRequest = defineRouteMiddleware(async (context, next) => {
	await next();

	const site = context.site;
	if (!site) return;

	const route = context.locals.starlightRoute;
	fillHead(route.head);
	const data = breadcrumbJsonLd(route, site, context.url.pathname);
	if (!data) return;

	route.head.push({
		tag: 'script',
		attrs: { type: 'application/ld+json' },
		content: jsonLdScript(data),
	});
});
