# Waitlist relay

Cloudflare Worker that accepts the encrypted waitlist POST and triggers
[`repository_dispatch`](https://docs.github.com/rest/repos/repos#create-a-repository-dispatch-event)
on `tomiok/persiaDB`. The static site cannot hold a GitHub token. The token lives here, as a
Worker secret.

Read [scripts/waitlist/README.md](../../../scripts/waitlist/README.md) for the keypair, the
`waitlist` branch, and decrypt. This file is only the deploy.

Requires Node.js 22+ and Wrangler 4.36 or newer (the rate-limit binding). Commands below pin
Wrangler `4.154.0`. Run them from this directory.

## 1. Token

Create a fine-grained personal access token:

- Repository access: only `tomiok/persiaDB`.
- Permission: **Contents: Read and write**.
- No Administration, Secrets, or Workflows.

GitHub documents Contents write as the permission for `POST /repos/{owner}/{repo}/dispatches`.
The token can also push code if it is stolen. Set an expiration.

## 2. Secret and deploy

```bash
npx --yes wrangler@4.154.0 login
npx --yes wrangler@4.154.0 secret put GITHUB_DISPATCH_TOKEN
npx --yes wrangler@4.154.0 deploy
```

`secret put` prompts for the token. Do not pass it as a shell argument if your shell records
history, and do not commit it. `.dev.vars` is gitignored. `.dev.vars.example` shows the name.

`GITHUB_REPOSITORY` and `ALLOWED_ORIGINS` are plain vars in `wrangler.toml`. The browser `Origin`
for GitHub Pages is `https://tomiok.github.io` (no path). `http://localhost:4321` and
`http://127.0.0.1:4321` are there so `astro dev` can call `wrangler dev`.

Local dry run:

```bash
cp .dev.vars.example .dev.vars
# edit .dev.vars and put the real token in it
npx --yes wrangler@4.154.0 dev
```

The deploy command prints an `https://<name>.<account>.workers.dev` URL. That URL is public. Set
it as the GitHub Actions variable `WAITLIST_RELAY_URL`, or as `defaultRelayUrl` in
`website/src/waitlist/config.mjs`. The site build reads the variable. The form stays closed until
the URL and the public key are both configured.

## What the Worker checks

- `POST` with `Content-Type: application/json`, and an `Origin` on the allowlist.
- Body at most 8192 bytes.
- JSON keys only `ciphertext` and `hp`.
- `hp` must be empty. A filled honeypot returns the same `{ "ok": true }` as a real submit and
  does not call GitHub.
- `ciphertext` is one canonical base64 line of at most 4096 characters and unpacks as a version-1
  `PWLE` envelope (512-byte wrapped key). The check is structural. The Worker cannot decrypt.
- Rate limit binding `WAITLIST_RATE_LIMIT`: 5 hits per 60 seconds per `CF-Connecting-IP`, per
  Cloudflare location. `namespace_id` is `"1001"`. Change it if that id is already used on the
  account. The limit is approximate and is not global across locations.
- Then `POST /repos/tomiok/persiaDB/dispatches` with `event_type` `waitlist-submission` and
  `client_payload.ciphertext` set to the canonical line.

GitHub Actions workflow `.github/workflows/waitlist.yml` checks the line again and appends it on
the `waitlist` branch. The workflow has to be on `main` before dispatch will run it, and Actions
needs permission to push that branch (see the other README).

## Tests

From the repository root:

```bash
node --test website/relay/waitlist/src/*.test.mjs
```
