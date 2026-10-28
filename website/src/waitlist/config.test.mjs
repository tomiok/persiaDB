import assert from 'node:assert/strict';
import test from 'node:test';

import { resolveRelayUrl, sanitizeRelayUrl } from './config.mjs';

test('relay URL allows https and local dev, and drops anything else', () => {
	assert.equal(sanitizeRelayUrl('https://persia-waitlist.example.workers.dev'), 'https://persia-waitlist.example.workers.dev');
	assert.equal(sanitizeRelayUrl('  http://localhost:4321/persiaDB  '), 'http://localhost:4321/persiaDB');
	assert.equal(sanitizeRelayUrl('http://127.0.0.1:8787'), 'http://127.0.0.1:8787');
	assert.equal(sanitizeRelayUrl('http://example.com/hook'), '');
	assert.equal(sanitizeRelayUrl('javascript:alert(1)'), '');
	assert.equal(sanitizeRelayUrl('https://example.com/<script>'), '');
	assert.equal(sanitizeRelayUrl(''), '');
	assert.equal(resolveRelayUrl(undefined), '');
	assert.equal(
		resolveRelayUrl('https://persia-waitlist.example.workers.dev/'),
		'https://persia-waitlist.example.workers.dev/',
	);
});
