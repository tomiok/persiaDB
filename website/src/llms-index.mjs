// llms.txt index for the docs site (https://llmstxt.org/).
//
// starlight-llms-txt (Astro 7 / Starlight 0.42) writes llms-full.txt and
// llms-small.txt, and it prefixes those file URLs with Astro's `base`. Its
// llms.txt does not list each docs page, so this module writes that index.
// The 404 page stays out of the index (it is noindex and not documentation).

import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const docsDir = path.join(here, 'content/docs');

/** GitHub Pages origin. `base` is the project-site path. */
export const origin = 'https://tomiok.github.io';
export const base = '/persiaDB';

/** Blockquote in llms.txt. Short, and explicit that the project is pre-alpha. */
export const summary =
	'embedded single-file search database in Rust, SQLite for search, pre-alpha';

export const llmsTxtUrl = `${origin}${base}/llms.txt`;
const llmsFullUrl = `${origin}${base}/llms-full.txt`;
const llmsSmallUrl = `${origin}${base}/llms-small.txt`;

// Sidebar order. A page that exists on disk but is missing here is still
// emitted, so a new doc cannot disappear from the index.
const docsOrder = ['index', 'features', 'architecture', 'roadmap', 'about'];
const guideOrder = [
	'guide/overview',
	'guide/embedded-search',
	'guide/comparison',
	'guide/getting-started',
	'guide/faq',
	'guide/for-agents',
	'guide/concepts',
	'guide/features',
	'guide/architecture',
	'guide/file-format',
	'guide/code',
	'guide/embedded-api',
	'guide/sdks',
	'guide/server',
	'guide/roadmap',
	'guide/contributing',
	'guide/license',
];

/**
 * @param {string} dir
 * @returns {string[]}
 */
function walk(dir) {
	/** @type {string[]} */
	const files = [];
	for (const name of readdirSync(dir)) {
		const full = path.join(dir, name);
		if (statSync(full).isDirectory()) files.push(...walk(full));
		else if (name.endsWith('.md') || name.endsWith('.mdx')) files.push(full);
	}
	return files;
}

/**
 * @param {string} file
 */
function slugFromFile(file) {
	const rel = path.relative(docsDir, file).split(path.sep).join('/');
	return rel.replace(/\.mdx?$/, '');
}

/**
 * @param {string} frontmatter
 * @param {string} key
 * @param {string} file
 */
function field(frontmatter, key, file) {
	const line = frontmatter.split('\n').find((entry) => entry.startsWith(`${key}:`));
	if (!line) throw new Error(`${file} is missing frontmatter ${key}`);
	let value = line.slice(key.length + 1).trim();
	if (
		(value.startsWith('"') && value.endsWith('"')) ||
		(value.startsWith("'") && value.endsWith("'"))
	) {
		value = value.slice(1, -1);
	}
	value = value.replaceAll('\\"', '"').replaceAll("\\'", "'").replace(/\s+/g, ' ').trim();
	if (value.length === 0) throw new Error(`${file} has an empty ${key}`);
	return value;
}

/**
 * @param {string} file
 */
function readPage(file) {
	const raw = readFileSync(file, 'utf8');
	const match = raw.match(/^---\r?\n([\s\S]*?)\r?\n---/);
	if (!match) throw new Error(`${file} is missing frontmatter`);
	const slug = slugFromFile(file);
	return {
		slug,
		title: field(match[1], 'title', file),
		description: field(match[1], 'description', file),
	};
}

/**
 * @param {string} slug
 */
function pageUrl(slug) {
	if (slug === 'index') return `${origin}${base}/`;
	return `${origin}${base}/${slug}/`;
}

/**
 * @param {Map<string, { slug: string, title: string, description: string }>} pages
 * @param {string[]} preferred
 * @param {string} label
 */
function ordered(pages, preferred, label) {
	for (const slug of preferred) {
		if (!pages.has(slug)) throw new Error(`llms.txt ${label} order lists missing page ${slug}`);
	}
	return preferred.map((slug) => {
		const page = pages.get(slug);
		if (!page) throw new Error(`llms.txt missing page ${slug}`);
		return page;
	});
}

/**
 * @param {{ title: string, description: string, slug: string }} page
 */
function linkLine(page) {
	const title = page.title.replaceAll(']', '\\]');
	return `- [${title}](${pageUrl(page.slug)}): ${page.description}`;
}

/** llmstxt.org index: H1, blockquote, then a link for every docs page. */
export function renderLlmsTxt() {
	const pages = new Map(walk(docsDir).map(readPage).map((page) => [page.slug, page]));
	pages.delete('404');

	const docs = ordered(pages, docsOrder, 'Docs');
	const guide = ordered(pages, guideOrder, 'Guide');
	const known = new Set([...docsOrder, ...guideOrder, '404']);
	const extra = [...pages.keys()]
		.filter((slug) => !known.has(slug))
		.sort()
		.map((slug) => {
			const page = pages.get(slug);
			if (!page) throw new Error(`llms.txt missing page ${slug}`);
			return page;
		});

	const sections = [
		'# Persia DB',
		`> ${summary}`,
		[
			'Pre-alpha. Nothing in the repository opens a database, indexes a document, or runs a search.',
			'`Db::open`, `put`, `commit`, and `search` in these pages are illustrative, from SPEC.md, and are not in the crate.',
			'SPEC.md wins if a page disagrees with it. PROGRESS.md is the live roadmap counter.',
			`The same pages, concatenated as Markdown, are [llms-full.txt](${llmsFullUrl}).`,
		].join(' '),
		['## Docs', docs.map(linkLine).join('\n')].join('\n\n'),
		['## Guide', guide.map(linkLine).join('\n')].join('\n\n'),
	];
	if (extra.length > 0) {
		sections.push(['## More', extra.map(linkLine).join('\n')].join('\n\n'));
	}
	sections.push(
		[
			'## Optional',
			[
				`- [Complete documentation](${llmsFullUrl}): every documentation page, concatenated as Markdown`,
				`- [Abridged documentation](${llmsSmallUrl}): the same pages, with asides and extra detail removed`,
				'- [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md): design contract. It wins if the site disagrees',
				'- [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md): planned work. Every leaf is one issue',
				'- [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md): live roadmap counter',
				'- [CLAUDE.md](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md): engineering rules for contributors',
			].join('\n'),
		].join('\n\n'),
	);

	const body = `${sections.join('\n\n')}\n`;
	assertIndex(body, pages);
	return body;
}

/**
 * @param {string} body
 * @param {Map<string, { slug: string }>} pages
 */
function assertIndex(body, pages) {
	if (!body.startsWith('# Persia DB\n\n')) throw new Error('llms.txt is missing the H1');
	if (!body.includes(`> ${summary}\n`)) throw new Error('llms.txt blockquote does not match the summary');
	for (const slug of pages.keys()) {
		const url = pageUrl(slug);
		if (!url.includes(`${base}/`)) throw new Error(`docs URL is missing the base: ${url}`);
		if (!body.includes(`](${url})`)) throw new Error(`llms.txt does not link ${url}`);
	}
	if (body.includes(`${origin}${base}/404`)) throw new Error('llms.txt links the 404 page');
	assertSiteUrls(body, 'llms.txt');
}

/**
 * Root-relative docs links become absolute and keep the project-site base.
 * Hash links and links to other hosts are left as they are.
 * @param {string} text
 */
export function absolutizeSiteLinks(text) {
	return text.replaceAll('](/persiaDB/', `](${origin}${base}/`).replaceAll('](/persiaDB)', `](${origin}${base}/)`);
}

/**
 * @param {string} text
 * @param {string} label
 */
export function assertSiteUrls(text, label) {
	const bad = [];
	for (const match of text.matchAll(/\]\(([^)]*)\)/g)) {
		const url = match[1];
		if (url.length === 0 || url.startsWith('#')) continue;
		if (url.startsWith(`${origin}/`)) {
			if (!url.startsWith(`${origin}${base}/`) && url !== `${origin}${base}`) bad.push(url);
			continue;
		}
		if (url.startsWith('/')) bad.push(url);
	}
	if (bad.length > 0) {
		throw new Error(`${label} has URLs that drop the ${base} base: ${bad.slice(0, 8).join(', ')}`);
	}
}

/** @returns {import('astro').AstroIntegration} */
export function persiaLlmsTxt() {
	return {
		name: 'persia-llms-txt',
		hooks: {
			'astro:server:setup': ({ server }) => {
				server.middlewares.use((req, res, next) => {
					const url = (req.url ?? '').split('?')[0];
					if (req.method !== 'GET' && req.method !== 'HEAD') {
						next();
						return;
					}
					if (url !== `${base}/llms.txt` && url !== '/llms.txt') {
						next();
						return;
					}
					const body = renderLlmsTxt();
					res.setHeader('Content-Type', 'text/plain; charset=utf-8');
					if (req.method === 'HEAD') res.end();
					else res.end(body);
				});
			},
			'astro:build:done': ({ dir, logger }) => {
				const dist = fileURLToPath(dir);
				const indexPath = path.join(dist, 'llms.txt');
				writeFileSync(indexPath, renderLlmsTxt());
				for (const name of ['llms-full.txt', 'llms-small.txt']) {
					const file = path.join(dist, name);
					if (!existsSync(file)) throw new Error(`starlight-llms-txt did not write ${name}`);
					const next = absolutizeSiteLinks(readFileSync(file, 'utf8'));
					assertSiteUrls(next, name);
					writeFileSync(file, next);
				}
				logger.info('Wrote llms.txt and made llms-full.txt links absolute under /persiaDB/.');
			},
		},
	};
}
