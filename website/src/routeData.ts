import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { breadcrumbJsonLd, jsonLdScript } from './breadcrumbs';

// Appended to the route `head` array Starlight already built from config and
// frontmatter, so the homepage SoftwareSourceCode JSON-LD, the FAQ JSON-LD,
// Open Graph tags, and the 404 noindex stay in place.
export const onRequest = defineRouteMiddleware(async (context, next) => {
	await next();

	const site = context.site;
	if (!site) return;

	const route = context.locals.starlightRoute;
	const data = breadcrumbJsonLd(route, site, context.url.pathname);
	if (!data) return;

	route.head.push({
		tag: 'script',
		attrs: { type: 'application/ld+json' },
		content: jsonLdScript(data),
	});
});
