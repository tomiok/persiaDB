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
links (`WinButton`) do the same with `import.meta.env.BASE_URL`.

`npm run build` writes the static site to `website/dist/`. `npm run preview` serves that build.
