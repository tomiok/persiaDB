// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { satteri } from '@astrojs/markdown-satteri';

// GitHub Pages project site: https://tomiok.github.io/persiaDB/
// `site` is the origin; `base` is the repository path (Astro's GitHub Pages guide).
const base = '/persiaDB';

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
    site: 'https://tomiok.github.io',
    base,
    integrations: [
        starlight({
            title: 'Persia DB',
            description:
                'An embedded, single-file, cloud-native search database, written in Rust. Pre-alpha.',
            customCss: [
                '@fontsource/jetbrains-mono/latin-400.css',
                '@fontsource/jetbrains-mono/latin-500.css',
                '@fontsource/jetbrains-mono/latin-600.css',
                './src/styles/site.css',
            ],
            expressiveCode: {
                themes: ['github-light', 'github-dark'],
            },
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
                        { label: 'Key concepts', slug: 'guide/concepts' },
                        { label: 'Features', slug: 'guide/features' },
                        { label: 'Architecture', slug: 'guide/architecture' },
                        { label: 'File format and storage', slug: 'guide/file-format' },
                        { label: 'Code', slug: 'guide/code' },
                        { label: 'Embedded API', slug: 'guide/embedded-api' },
                        { label: 'Server, cloud, and Kubernetes', slug: 'guide/server' },
                        { label: 'Roadmap', slug: 'guide/roadmap' },
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
