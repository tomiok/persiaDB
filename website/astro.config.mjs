// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { satteri } from '@astrojs/markdown-satteri';
import starlightLlmsTxt from 'starlight-llms-txt';

// GitHub Pages project site: https://tomiok.github.io/persiaDB/
// `site` is the origin; `base` is the repository path (Astro's GitHub Pages guide).
const origin = 'https://tomiok.github.io';
const base = '/persiaDB';

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
            head: [
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
                starlightLlmsTxt({
                    projectName: 'Persia DB',
                    description:
                        'Pre-alpha embedded search database written in Rust. Nothing opens a database, indexes, or searches yet. persia-format implements on-disk codecs only. Db::open and search in the docs are illustrative, from SPEC.md, and are not in the crate.',
                    details:
                        'SPEC.md wins if a page disagrees with it. PROGRESS.md is the live roadmap counter. Do not call Db, Collection, put, commit, or search: those items are not in the repository. The intended usage and the FAQ are the Getting started page. Constraints for patches are the For AI agents page and CLAUDE.md.',
                    promote: [
                        'index*',
                        'guide/getting-started*',
                        'guide/for-agents*',
                        'guide/overview*',
                    ],
                    optionalLinks: [
                        {
                            label: 'Getting started',
                            url: 'https://tomiok.github.io/persiaDB/guide/getting-started/',
                            description:
                                'What you can call today, and the illustrative open, schema, commit, and search API',
                        },
                        {
                            label: 'For AI agents',
                            url: 'https://tomiok.github.io/persiaDB/guide/for-agents/',
                            description: 'Status, constraints, and the calls that are not in the crate',
                        },
                        {
                            label: 'Embedded SDKs',
                            url: 'https://tomiok.github.io/persiaDB/guide/sdks/',
                            description:
                                'Planned in-process Go and TypeScript bindings. Not implemented. The network clients are a different milestone.',
                        },
                        {
                            label: 'SPEC.md',
                            url: 'https://github.com/tomiok/persiaDB/blob/main/SPEC.md',
                            description: 'Design contract. It wins if the site disagrees',
                        },
                        {
                            label: 'PROGRESS.md',
                            url: 'https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md',
                            description: 'Live roadmap counter',
                        },
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
                        { label: 'Comparison', slug: 'guide/comparison' },
                        { label: 'Getting started', slug: 'guide/getting-started' },
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
    ],
    markdown: {
        processor: satteri({ mdastPlugins: prefixBaseLinks }),
    },
});
