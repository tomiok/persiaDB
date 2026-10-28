# Waitlist

The site is static. The form in the browser encrypts a submission with a public key, and a
Cloudflare Worker asks GitHub to append that ciphertext to `waitlist.txt.enc` on the `waitlist`
branch. There is no database. The private key never goes in the repository, the site, or the worker.

This is not the Persia DB on-disk format. It is a line-oriented file in git.

## Transport

A visitor does not need a GitHub account. The page POSTs the ciphertext to a Worker you deploy.
The Worker holds a GitHub token as a secret and calls
[`repository_dispatch`](https://docs.github.com/rest/repos/repos#create-a-repository-dispatch-event).
The token is not in the HTML or the JavaScript.

Opening a prefilled GitHub issue would also avoid putting a token in the page, but the visitor
would need a GitHub account. The relay is the path that does not.

The dispatch workflow is [`.github/workflows/waitlist.yml`](../../.github/workflows/waitlist.yml).
GitHub only runs that file from the default branch, so submissions start working after this is
merged to `main`.

## Why a `waitlist` branch

Each signup is a commit. Putting those commits on `main` would redeploy the site, rerun CI, and
fill the project history. The workflow creates `waitlist` on the first accepted submission and
only updates `waitlist.txt.enc` there. Do not merge `waitlist` into `main`.

## What you do once

### 1. Generate the keypair

From the repository root, on your machine:

```bash
node scripts/waitlist/keygen.mjs
```

That writes:

- `website/src/waitlist/public.pem` — commit this. It replaces the placeholder.
- `~/.config/persia/waitlist-private.pem` — mode `0600`. Leave it here. Do not commit it.

`keygen` refuses to write the private key inside the repository. Pass `--private` and `--public`
to choose other paths. Pass `--force` to overwrite a key that already exists. Overwriting the
public key makes older lines undecryptable with the new key, so keep the old private key if you
still want to read them.

Node.js 22 or newer (the same major the site uses).

### 2. Create a GitHub token

Fine-grained personal access token:

- Only the `tomiok/persiaDB` repository.
- Repository permission **Contents: Read and write**.
- No Administration, Secrets, or Workflows permission.

GitHub requires Contents write for `POST /repos/{owner}/{repo}/dispatches`. A classic PAT with the
`repo` scope also works; the fine-grained token is the narrower choice, and it is still able to
push to the repo if it leaks. Set an expiration and rotate it.

### 3. Deploy the Worker

See [website/relay/waitlist/README.md](../../website/relay/waitlist/README.md). The short version,
from `website/relay/waitlist`:

```bash
npx --yes wrangler@4.154.0 login
npx --yes wrangler@4.154.0 secret put GITHUB_DISPATCH_TOKEN
npx --yes wrangler@4.154.0 deploy
```

Paste the token when `secret put` asks. Wrangler stores it on Cloudflare. It does not go in the
repo. Note the `https://….workers.dev` URL from the deploy output.

### 4. Point the site at the Worker

Either set the GitHub Actions variable `WAITLIST_RELAY_URL` to that URL
(Settings → Secrets and variables → Actions → Variables), or set `defaultRelayUrl` in
[`website/src/waitlist/config.mjs`](../../website/src/waitlist/config.mjs) and commit it.

The website workflow passes `WAITLIST_RELAY_URL` into the build as `PUBLIC_WAITLIST_RELAY_URL`.
A non-empty variable wins over `defaultRelayUrl`. The URL is public. It is not a secret.

The form stays disabled until **both** a real public key and a relay URL are present at build
time. Commit the public key, set the URL, and let the site deploy.

### 5. Let Actions push the `waitlist` branch

Settings → Actions → General → Workflow permissions → **Read and write permissions**.

Do not protect `waitlist` in a way that blocks `github-actions[bot]`. The workflow pushes there
with the built-in `GITHUB_TOKEN`. It does not use the personal access token.

## Decrypt

```bash
git fetch origin waitlist
node scripts/waitlist/decrypt.mjs --key ~/.config/persia/waitlist-private.pem --from-git origin/waitlist
```

Or copy the file and pass `--file`:

```bash
git show origin/waitlist:waitlist.txt.enc > /tmp/waitlist.txt.enc
node scripts/waitlist/decrypt.mjs --key ~/.config/persia/waitlist-private.pem --file /tmp/waitlist.txt.enc
```

One JSON object per line on stdout. A line that does not decrypt is reported on stderr, and the
process exits non-zero if any line failed. `PERSIA_WAITLIST_PRIVATE_KEY` can replace `--key`.

## Envelope

Each line of `waitlist.txt.enc` is standard base64 (with padding, no whitespace) of:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `PWLE` |
| 4 | 1 | version `1` |
| 5 | 2 | wrapped-key length, uint16 little-endian, must be `512` |
| 7 | 512 | RSA-4096 OAEP-SHA-256 wrapping of a 32-byte AES-256 key |
| 519 | 12 | AES-GCM IV |
| 531 | rest | AES-GCM ciphertext, including the 16-byte tag |

AES-GCM additional data is the UTF-8 string `persia-waitlist-v1`. The plaintext is JSON:

```json
{"v":1,"email":"a@b.co","name":"","company":"","use_case":"","submitted_at":"2026-10-28T00:00:00.000Z"}
```

The worker and the Action both reject a line that is not this shape. The cap is 4096 characters.
They do not decrypt. Field limits before encryption: email 254, name 120, company 120, use case
500 characters. The UTF-8 JSON must fit in 2048 bytes.

## Tests

```bash
node --test \
  website/src/waitlist/*.test.mjs \
  website/src/waitlist/protocol/*.test.mjs \
  website/relay/waitlist/src/*.test.mjs \
  scripts/waitlist/*.test.mjs
```

The website workflow runs the same command. `cd website && npm test` runs the site and relay tests
only.

## Limits

- Anyone can encrypt to the public key. The ciphertext does not prove who sent it. A hidden
  honeypot field and the rate limit (5 requests per 60 seconds per IP, counted separately in each
  Cloudflare location) cut down noise. They are not a global daily cap, and Cloudflare's limit is
  approximate. Junk still has to be ignored after decrypt.
- The Worker secret can push to the repository, because that is the permission GitHub requires for
  dispatch. Keep it on the one repository, expire it, and rotate it.
- There is no forward secrecy for lines already stored. The old private key still opens them.
- The form needs JavaScript. Without it, the page does not submit.
- The first accepted submission creates the `waitlist` branch. Until then, `git fetch origin waitlist`
  fails, which is expected.
- Retries of the exact same ciphertext are not stored twice. A new submission uses a new AES key,
  so two visitors do not collide.
