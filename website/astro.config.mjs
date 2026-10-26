// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { satteri } from '@astrojs/markdown-satteri';
import starlightLlmsTxt from 'starlight-llms-txt';
import { base, llmsTxtUrl, origin, persiaLlmsTxt, summary } from './src/llms-index.mjs';
import { progressMarkdownPlugin } from './src/progress-plugin.mjs';

// GitHub Pages project site: https://tomiok.github.io/persiaDB/
// `site` is the origin; `base` is the repository path (Astro's GitHub Pages guide).

// Crawlers ignore relative og:image URLs. Keep these absolute and under `base`.
const ogImage = `${origin}${base}/og.png`;
const ogAlt = 'Persia DB. Think SQLite for search.';
const jsonLd = JSON.stringify({
    '@context': 'https://schema.org',
    '@type': 'SoftwareSourceCode',
    name: 'Persia DB',
    description: 'An embedded, single-file, cloud-native search database, written in Rust.',
    codeRepository: 'https://github.com/tomiok/persiaDB',
    programmingLanguage: 'Rust',
    license: 'https://www.apache.org/licenses/LICENSE-2.0',
    url: `${origin}${base}/`,
});

// Starlight leaves markdown links that start with `/` untouched, so they miss `base`
// and 404 on GitHub Pages. JSX (`WinButton`) prefixes `BASE_URL` itself.
const prefix = base.replace(/\/$/, '');

/**
 * @param {string} url
 */
function withBase(url) {
    if (
        url.startsWith('/') &&
        !url.startsWith('//') &&
        url !== prefix &&
        !url.startsWith(`${prefix}/`)
    ) {
        return prefix + url;
    }
    return url;
}

/** @type {import('@astrojs/markdown-satteri').SatteriProcessorOptions['mdastPlugins']} */
const prefixBaseLinks = [
    progressMarkdownPlugin,
    {
        name: 'persia-prefix-base',
        link(node, context) {
            const url = withBase(node.url);
            if (url !== node.url) context.setProperty(node, 'url', url);
        },
        image(node, context) {
            const url = withBase(node.url);
            if (url !== node.url) context.setProperty(node, 'url', url);
        },
        definition(node, context) {
            const url = withBase(node.url);
            if (url !== node.url) context.setProperty(node, 'url', url);
        },
    },
];

export default defineConfig({
    site: origin,
    base,
    // Starlight serves docs/404.md from its own 404 route, and the docs
    // collection also emits that path. The dedicated route wins; ignore the
    // expected collision instead of warning on every build.
    prerenderConflictBehavior: 'ignore',
    integrations: [
        starlight({
            title: 'Persia DB',
            description:
                'An embedded, single-file, cloud-native search database, written in Rust. Pre-alpha.',
            // File paths are relative to this package, so the repo prefix is `website/`.
            editLink: {
                baseUrl: 'https://github.com/tomiok/persiaDB/edit/main/website/',
            },
            // Dates come from git log. The Pages workflow checks out the full history.
            lastUpdated: true,
            // BreadcrumbList JSON-LD. The homepage and 404 are skipped in the middleware.
            routeMiddleware: './src/routeData.ts',
            head: [
                // llmstxt.org: agents look for this hint. The href is absolute and includes `base`.
                {
                    tag: 'link',
                    attrs: {
                        rel: 'alternate',
                        type: 'text/plain',
                        href: llmsTxtUrl,
                        title: 'llms.txt',
                    },
                },
                { tag: 'link', attrs: { rel: 'describedby', href: llmsTxtUrl } },
                { tag: 'meta', attrs: { property: 'og:image', content: ogImage } },
                { tag: 'meta', attrs: { property: 'og:image:alt', content: ogAlt } },
                { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
                { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
                { tag: 'meta', attrs: { property: 'og:image:type', content: 'image/png' } },
                { tag: 'meta', attrs: { name: 'twitter:image', content: ogImage } },
                { tag: 'meta', attrs: { name: 'twitter:image:alt', content: ogAlt } },
                { tag: 'script', attrs: { type: 'application/ld+json' }, content: jsonLd },
            ],
            customCss: [
                '@fontsource/jetbrains-mono/latin-400.css',
                '@fontsource/jetbrains-mono/latin-500.css',
                '@fontsource/jetbrains-mono/latin-600.css',
                './src/styles/site.css',
            ],
            expressiveCode: {
                themes: ['github-light', 'github-dark'],
            },
            components: {
                Footer: './src/components/SiteFooter.astro',
            },
            plugins: [
                // Writes llms-full.txt and llms-small.txt (Markdown of the pages).
                // persiaLlmsTxt() replaces llms.txt so the index lists every page.
                starlightLlmsTxt({
                    projectName: 'Persia DB',
                    description: summary,
                    promote: [
                        'index*',
                        'guide/getting-started*',
                        'guide/faq*',
                        'guide/for-agents*',
                        'guide/overview*',
                        'guide/embedded-search*',
                    ],
                }),
            ],
            social: [
                {
                    icon: 'github',
                    label: 'GitHub',
                    href: 'https://github.com/tomiok/persiaDB',
                },
            ],
            sidebar: [
                { label: 'Home', link: '/' },
                { label: 'Features', slug: 'features' },
                { label: 'How it works', slug: 'architecture' },
                { label: 'Roadmap', slug: 'roadmap' },
                { label: 'About', slug: 'about' },
                {
                    label: 'Guide',
                    items: [
                        { label: 'Overview', slug: 'guide/overview' },
                        { label: 'Embedded search', slug: 'guide/embedded-search' },
                        { label: 'Comparison', slug: 'guide/comparison' },
                        { label: 'Getting started', slug: 'guide/getting-started' },
                        { label: 'FAQ', slug: 'guide/faq' },
                        { label: 'For AI agents', slug: 'guide/for-agents' },
                        { label: 'Key concepts', slug: 'guide/concepts' },
                        { label: 'Feature reference', slug: 'guide/features' },
                        { label: 'Crate architecture', slug: 'guide/architecture' },
                        { label: 'File format and storage', slug: 'guide/file-format' },
                        { label: 'Code', slug: 'guide/code' },
                        { label: 'Embedded API', slug: 'guide/embedded-api' },
                        { label: 'Embedded SDKs', slug: 'guide/sdks' },
                        { label: 'Server, cloud, and Kubernetes', slug: 'guide/server' },
                        { label: 'Milestones', slug: 'guide/roadmap' },
                        { label: 'Contributing', slug: 'guide/contributing' },
                        { label: 'License', slug: 'guide/license' },
                    ],
                },
            ],
        }),
        // Runs after Starlight so it can replace the plugin's llms.txt and
        // rewrite root-relative links inside llms-full.txt.
        persiaLlmsTxt(),
    ],
    markdown: {
        processor: satteri({ mdastPlugins: prefixBaseLinks }),
    },
});
