import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { breadcrumbJsonLd, jsonLdScript } from './breadcrumbs';
import { substituteStatusText } from './status.mjs';

// Frontmatter `head` (the FAQ JSON-LD) is not Markdown, so the progress plugin
// never sees it. Substitute the same sentences the pages use.
function fillProgress(value: string): string {
	return substituteStatusText(value);
}

type HeadEntry = { tag: string; content?: unknown; attrs?: Record<string, unknown> };

function fillHead(head: HeadEntry[]): void {
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

// The header site title stays "Persia DB" (Starlight `title` is also the visible
// name). Search titles use the queried spelling: Starlight composes
// "<page> | Persia DB", and this rewrites only that suffix, plus og:site_name.
// A frontmatter `<title>` that already ends in "PersiaDB" is left as written.
const VISIBLE_BRAND = 'Persia DB';
const SEO_BRAND = 'PersiaDB';
const COMPOSED_SUFFIX = ` | ${VISIBLE_BRAND}`;

function useSearchBrand(value: string): string {
	if (!value.endsWith(COMPOSED_SUFFIX)) return value;
	return `${value.slice(0, -VISIBLE_BRAND.length)}${SEO_BRAND}`;
}

function metaContent(head: HeadEntry[], key: 'property' | 'name', name: string): string | undefined {
	for (const entry of head) {
		if (entry.tag !== 'meta' || entry.attrs?.[key] !== name) continue;
		const content = entry.attrs.content;
		return typeof content === 'string' ? content : undefined;
	}
	return undefined;
}

function setMeta(head: HeadEntry[], key: 'property' | 'name', name: string, content: string): void {
	for (const entry of head) {
		if (entry.tag !== 'meta' || entry.attrs?.[key] !== name) continue;
		entry.attrs.content = content;
		return;
	}
	head.push({ tag: 'meta', attrs: { [key]: name, content } });
}

function applySearchBrand(head: HeadEntry[]): void {
	for (const entry of head) {
		if (entry.tag === 'title' && typeof entry.content === 'string') {
			entry.content = useSearchBrand(entry.content);
		}
		const attrs = entry.attrs;
		if (entry.tag !== 'meta' || !attrs) continue;
		if (attrs.property === 'og:site_name' && attrs.content === VISIBLE_BRAND) {
			attrs.content = SEO_BRAND;
		}
		const socialTitle = attrs.property === 'og:title' || attrs.name === 'twitter:title';
		if (socialTitle && typeof attrs.content === 'string') {
			attrs.content = useSearchBrand(attrs.content);
		}
	}

	// Starlight's og:title is the short heading. Social titles follow the
	// document title, including a frontmatter override or the PersiaDB suffix.
	const title = head.find((entry) => entry.tag === 'title');
	if (typeof title?.content !== 'string') return;
	if (metaContent(head, 'property', 'og:title') !== title.content) {
		setMeta(head, 'property', 'og:title', title.content);
	}
	if (metaContent(head, 'name', 'twitter:title') !== title.content) {
		setMeta(head, 'name', 'twitter:title', title.content);
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
	applySearchBrand(route.head);
	const data = breadcrumbJsonLd(route, site, context.url.pathname);
	if (!data) return;

	route.head.push({
		tag: 'script',
		attrs: { type: 'application/ld+json' },
		content: jsonLdScript(data),
	});
});
