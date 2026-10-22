import type { StarlightRouteData } from '@astrojs/starlight/route-data';

type SidebarEntry = StarlightRouteData['sidebar'][number];
type SidebarGroup = Extract<SidebarEntry, { type: 'group' }>;
type SidebarLink = Extract<SidebarEntry, { type: 'link' }>;

/** One step in a BreadcrumbList. `item` is omitted when the step is not a page. */
interface Crumb {
	name: string;
	item?: string;
}

interface Trail {
	groups: SidebarGroup[];
	link: SidebarLink;
}

/**
 * BreadcrumbList for one docs page.
 *
 * The trail follows the sidebar: Home, then each ancestor group label, then the
 * page title. A group is a label in the sidebar, not a URL, unless it is a link.
 * Home and the current page always carry absolute URLs (`site` + `base`).
 * Returns undefined for the homepage and the 404 page.
 */
export function breadcrumbJsonLd(
	route: StarlightRouteData,
	site: URL,
	pathname: string,
): Record<string, unknown> | undefined {
	if (isNotFound(route, pathname) || isHome(route, site, pathname)) return undefined;

	const pageUrl = canonicalUrl(route) ?? absoluteUrl(pathname, site);
	if (samePath(pageUrl, absoluteUrl(route.siteTitleHref, site), site)) return undefined;

	const title = route.entry.data.title.trim();
	if (title.length === 0) return undefined;

	const trail = findTrail(route.sidebar);
	const crumbs: Crumb[] = [
		{
			name: homeLabel(route.sidebar, route.siteTitleHref, site),
			item: absoluteUrl(route.siteTitleHref, site),
		},
	];

	// Ancestor groups are the section labels in the sidebar ("Guide"). Include a
	// URL only when that entry is itself a page. Pointing "Guide" at Overview
	// would publish a URL for a different page.
	for (const group of trail?.groups ?? []) {
		const name = group.label.trim();
		if (name.length === 0) continue;
		crumbs.push({ name });
	}

	crumbs.push({ name: title, item: pageUrl });
	if (crumbs.length < 2) return undefined;

	return {
		'@context': 'https://schema.org',
		'@type': 'BreadcrumbList',
		itemListElement: crumbs.map((crumb, index) => {
			const element: Record<string, unknown> = {
				'@type': 'ListItem',
				position: index + 1,
				name: crumb.name,
			};
			if (crumb.item) element.item = crumb.item;
			return element;
		}),
	};
}

/** JSON-LD safe to drop into a `<script>` via `set:html` (`<` cannot close the tag). */
export function jsonLdScript(data: Record<string, unknown>): string {
	return JSON.stringify(data).replace(/</g, '\\u003c');
}

function isNotFound(route: StarlightRouteData, pathname: string): boolean {
	if (isNotFoundId(route.id) || isNotFoundId(route.entry.id)) return true;
	const path = stripTrailingSlash(pathname);
	return path.endsWith('/404') || path.endsWith('/404.html');
}

function isNotFoundId(id: string): boolean {
	const bare = id.replace(/\.mdx?$/, '');
	return bare === '404' || bare.endsWith('/404');
}

function isHome(route: StarlightRouteData, site: URL, pathname: string): boolean {
	if (isHomeId(route.id) || isHomeId(route.entry.id)) return true;
	return samePath(pathname, route.siteTitleHref, site);
}

function isHomeId(id: string): boolean {
	const bare = id.replace(/\.mdx?$/, '');
	return bare === '' || bare === 'index' || bare === '/';
}

function canonicalUrl(route: StarlightRouteData): string | undefined {
	for (const tag of route.head) {
		if (tag.tag !== 'link' || tag.attrs?.rel !== 'canonical') continue;
		const href = tag.attrs.href;
		if (typeof href === 'string' && href.length > 0) return href;
	}
	return undefined;
}

function homeLabel(sidebar: readonly SidebarEntry[], homeHref: string, site: URL): string {
	const link = flattenLinks(sidebar).find((entry) => samePath(entry.href, homeHref, site));
	const label = link?.label.trim();
	return label && label.length > 0 ? label : 'Home';
}

function findTrail(entries: readonly SidebarEntry[], groups: SidebarGroup[] = []): Trail | undefined {
	for (const entry of entries) {
		if (entry.type === 'link') {
			if (entry.isCurrent) return { groups, link: entry };
			continue;
		}
		const nested = findTrail(entry.entries, [...groups, entry]);
		if (nested) return nested;
	}
	return undefined;
}

function flattenLinks(entries: readonly SidebarEntry[]): SidebarLink[] {
	const links: SidebarLink[] = [];
	for (const entry of entries) {
		if (entry.type === 'link') links.push(entry);
		else links.push(...flattenLinks(entry.entries));
	}
	return links;
}

function absoluteUrl(href: string, site: URL): string {
	return new URL(href, site).href;
}

/** Compare pathnames, ignoring trailing slash and a directory `index.html`. */
function samePath(a: string, b: string, site: URL): boolean {
	return normalizePath(a, site) === normalizePath(b, site);
}

function normalizePath(href: string, site: URL): string {
	const path = new URL(href, site).pathname;
	const withoutIndex = path.endsWith('/index.html') ? path.slice(0, -'index.html'.length) : path;
	return stripTrailingSlash(withoutIndex) || '/';
}

function stripTrailingSlash(path: string): string {
	if (path.length > 1 && path.endsWith('/')) return path.slice(0, -1);
	return path;
}
