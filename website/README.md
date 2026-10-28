# Persia DB website

Astro Starlight site for the project. It is not part of the Cargo workspace: there is no
`Cargo.toml` here, and the Rust CI jobs do not build it.

The guide pages under `src/content/docs/guide/` are the sections of [`docs/guide.md`](../docs/guide.md).
[SPEC.md](../SPEC.md) stays the design contract. When a page and the SPEC disagree, the SPEC wins.

Production URL, once GitHub Pages is enabled for this repository: <https://tomiok.github.io/persiaDB/>.

## Preview locally

Requires Node.js 22 or newer.

```bash
cd website
npm install
npm run dev
```

The dev server prints a URL. Because the site is configured with `base: '/persiaDB'`, open

<http://localhost:4321/persiaDB/>

Markdown links that start with `/` are prefixed with that base at build time. Component
links do the same with `import.meta.env.BASE_URL`.

`npm run build` writes the static site to `website/dist/`. `npm run preview` serves that build.
The same build writes [`llms.txt`](https://tomiok.github.io/persiaDB/llms.txt) (an index of every docs page) and
[`llms-full.txt`](https://tomiok.github.io/persiaDB/llms-full.txt) (those pages in Markdown). Links in both files
use the `/persiaDB/` base.

## Waitlist

The landing page and `/waitlist/` include a form that encrypts the submission in the browser.
Setup (keypair, Cloudflare Worker, GitHub secret) is [`scripts/waitlist/README.md`](../scripts/waitlist/README.md).
The relay that forwards ciphertext lives in [`relay/waitlist`](relay/waitlist/README.md).

```bash
npm test
```
