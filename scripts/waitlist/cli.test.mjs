import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { WRAPPED_KEY_LEN } from '../../website/src/waitlist/protocol/constants.mjs';
import { encodeLine, packEnvelope } from '../../website/src/waitlist/protocol/envelope.mjs';
import { encryptSubmission } from '../../website/src/waitlist/protocol/crypto.mjs';
import { assertPrivateKeyDestination, findRepoRoot, isInsideRepo } from './keygen.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = findRepoRoot(here);
const node = process.execPath;

function synthetic() {
	return encodeLine(
		packEnvelope({
			wrappedKey: new Uint8Array(WRAPPED_KEY_LEN).fill(9),
			iv: new Uint8Array(12).fill(2),
			ciphertext: new Uint8Array(16).fill(3),
		}),
	);
}

test('private key destination cannot be inside the repository', () => {
	assert.ok(repo);
	assert.equal(isInsideRepo(path.join(repo, 'website/src/waitlist/public.pem'), repo), true);
	assert.throws(
		() => assertPrivateKeyDestination(path.join(repo, 'waitlist-private.pem'), repo),
		/refusing to write a private key/,
	);
	const outside = path.join(os.tmpdir(), 'persia-waitlist-private.pem');
	assert.equal(isInsideRepo(outside, repo), false);
	assert.doesNotThrow(() => assertPrivateKeyDestination(outside, repo));
});

test('keygen refuses to write the private key into the repo', () => {
	assert.ok(repo);
	const result = spawnSync(
		node,
		[
			path.join(here, 'keygen.mjs'),
			'--public',
			path.join(os.tmpdir(), 'persia-waitlist-should-not-write.pem'),
			'--private',
			path.join(repo, 'waitlist-private.pem'),
		],
		{ encoding: 'utf8' },
	);
	assert.notEqual(result.status, 0);
	assert.match(result.stderr, /refusing/);
});

test('merge CLI appends once and reports a duplicate', () => {
	const dir = mkdtempSync(path.join(os.tmpdir(), 'persia-waitlist-merge-'));
	const current = path.join(dir, 'current.txt');
	const out = path.join(dir, 'out.txt');
	writeFileSync(current, '# note\n');
	const line = synthetic();
	const first = spawnSync(node, [path.join(here, 'merge.mjs'), '--current', current, '--out', out], {
		encoding: 'utf8',
		env: { ...process.env, WAITLIST_CIPHERTEXT: line },
	});
	assert.equal(first.status, 0, first.stderr);
	assert.equal(readFileSync(out, 'utf8'), `# note\n${line}\n`);
	const second = spawnSync(node, [path.join(here, 'merge.mjs'), '--current', out, '--out', out], {
		encoding: 'utf8',
		env: { ...process.env, WAITLIST_CIPHERTEXT: line },
	});
	assert.equal(second.status, 2, second.stderr);
	const bad = spawnSync(node, [path.join(here, 'merge.mjs'), '--current', current, '--out', out], {
		encoding: 'utf8',
		env: { ...process.env, WAITLIST_CIPHERTEXT: 'nope' },
	});
	assert.equal(bad.status, 1);
	assert.match(bad.stderr, /invalid waitlist ciphertext/);
	assert.doesNotMatch(bad.stderr, /nope/);
});

test('keygen and decrypt CLIs round-trip a submission', { timeout: 120_000 }, async () => {
	const dir = mkdtempSync(path.join(os.tmpdir(), 'persia-waitlist-keys-'));
	const publicPath = path.join(dir, 'public.pem');
	const privatePath = path.join(dir, 'private.pem');
	const generated = spawnSync(
		node,
		[path.join(here, 'keygen.mjs'), '--public', publicPath, '--private', privatePath],
		{ encoding: 'utf8' },
	);
	assert.equal(generated.status, 0, generated.stderr);
	const mode = statSync(privatePath).mode & 0o777;
	assert.equal(mode, 0o600);
	assert.match(readFileSync(publicPath, 'utf8'), /BEGIN PUBLIC KEY/);
	assert.doesNotMatch(readFileSync(publicPath, 'utf8'), /BEGIN PRIVATE KEY/);

	const again = spawnSync(
		node,
		[path.join(here, 'keygen.mjs'), '--public', publicPath, '--private', privatePath],
		{ encoding: 'utf8' },
	);
	assert.notEqual(again.status, 0);
	assert.match(again.stderr, /already exists/);

	const line = await encryptSubmission(readFileSync(publicPath, 'utf8'), {
		v: 1,
		email: 'a@b.co',
		name: '',
		company: '',
		use_case: '',
		submitted_at: '2026-10-28T00:00:00.000Z',
	});
	const enc = path.join(dir, 'waitlist.txt.enc');
	writeFileSync(enc, `# comment\n${line}\nnot-a-line\n`);
	const decrypted = spawnSync(
		node,
		[path.join(here, 'decrypt.mjs'), '--key', privatePath, '--file', enc],
		{ encoding: 'utf8' },
	);
	assert.equal(decrypted.status, 1);
	assert.match(decrypted.stderr, /could not decrypt/);
	assert.equal(JSON.parse(decrypted.stdout).email, 'a@b.co');
});
