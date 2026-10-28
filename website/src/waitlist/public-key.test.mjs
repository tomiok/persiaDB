import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { importPublicKey, looksLikeSpkiPem } from './protocol/crypto.mjs';

const pem = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), 'public.pem'), 'utf8');

test('the committed waitlist key file is not a private key', async () => {
	assert.doesNotMatch(pem, /BEGIN PRIVATE KEY/);
	assert.doesNotMatch(pem, /BEGIN RSA PRIVATE KEY/);
	if (looksLikeSpkiPem(pem)) {
		const key = await importPublicKey(pem);
		assert.equal(key.type, 'public');
	} else {
		assert.match(pem, /not a key/);
	}
});
