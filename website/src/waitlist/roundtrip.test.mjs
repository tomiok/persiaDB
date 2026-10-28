import assert from 'node:assert/strict';
import test from 'node:test';

import { generateKeyPairPem } from '../../../scripts/waitlist/keygen.mjs';
import { base64ToBytes } from './protocol/bytes.mjs';
import { decryptLine } from './protocol/crypto.mjs';
import { assertCiphertextLine } from './protocol/envelope.mjs';
import { prepareSubmission } from './submit.mjs';
import { handleRequest } from '../../relay/waitlist/src/index.mjs';

const SEED = 0x50454e44;

/** @param {number} seed */
function mulberry32(seed) {
	let state = seed >>> 0;
	return () => {
		state = (state + 0x6d2b79f5) >>> 0;
		let t = Math.imul(state ^ (state >>> 15), 1 | state);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

/**
 * @param {Uint8Array} haystack
 * @param {Uint8Array} needle
 */
function includesBytes(haystack, needle) {
	if (needle.length === 0 || needle.length > haystack.length) return false;
	for (let i = 0; i <= haystack.length - needle.length; i++) {
		let match = true;
		for (let j = 0; j < needle.length; j++) {
			if (haystack[i + j] !== needle[j]) {
				match = false;
				break;
			}
		}
		if (match) return true;
	}
	return false;
}

test('encrypt, relay validation, and decrypt round-trip', { timeout: 120_000 }, async () => {
	const { publicKey, privateKey } = generateKeyPairPem();
	const now = new Date('2026-10-28T15:04:00.000Z');
	const prepared = await prepareSubmission({
		publicKeyPem: publicKey,
		fields: {
			email: 'Ada@Example.com',
			name: 'Ada',
			company: 'Analytical',
			useCase: 'sqlite for search',
			hp: '',
		},
		now,
	});
	assert.equal(prepared.status, 'ready');
	if (prepared.status !== 'ready') return;
	const line = prepared.body.ciphertext;
	assert.equal(assertCiphertextLine(line), line);
	const emailBytes = new TextEncoder().encode('ada@example.com');
	assert.equal(includesBytes(base64ToBytes(line), emailBytes), false);

	/** @type {string[]} */
	const dispatched = [];
	const response = await handleRequest(
		fakeRequest(JSON.stringify(prepared.body)),
		{
			ALLOWED_ORIGINS: 'https://tomiok.github.io',
			GITHUB_REPOSITORY: 'tomiok/persiaDB',
			GITHUB_DISPATCH_TOKEN: 'github_pat_roundtrip_token_123456',
			WAITLIST_RATE_LIMIT: { async limit() { return { success: true }; } },
		},
		{
			async fetch(_url, init) {
				dispatched.push(JSON.parse(String(init.body)).client_payload.ciphertext);
				return new Response(null, { status: 204 });
			},
		},
	);
	assert.equal(response.status, 200);
	assert.deepEqual(dispatched, [line]);

	const decoded = await decryptLine(privateKey, line);
	assert.equal(decoded.email, 'ada@example.com');
	assert.equal(decoded.name, 'Ada');
	assert.equal(decoded.use_case, 'sqlite for search');
	assert.equal(decoded.submitted_at, '2026-10-28T15:04:00.000Z');

	const tamperedBytes = base64ToBytes(line);
	tamperedBytes[tamperedBytes.length - 1] ^= 0xff;
	const tampered = Buffer.from(tamperedBytes).toString('base64');
	await assert.rejects(() => decryptLine(privateKey, tampered));

	const other = generateKeyPairPem();
	await assert.rejects(() => decryptLine(other.privateKey, line));
});

test('random submissions round-trip', { timeout: 120_000 }, async () => {
	const { publicKey, privateKey } = generateKeyPairPem();
	const rand = mulberry32(SEED);
	for (let i = 0; i < 8; i++) {
		const local = `user${Math.floor(rand() * 1000)}`;
		const email = `${local}@example.com`;
		const useCase = rand() > 0.5 ? `notes ${i}\nline` : '';
		const prepared = await prepareSubmission({
			publicKeyPem: publicKey,
			fields: { email, name: rand() > 0.5 ? 'N' : '', company: '', useCase, hp: '' },
			now: new Date('2026-10-28T00:00:00.000Z'),
		});
		assert.equal(prepared.status, 'ready');
		if (prepared.status !== 'ready') continue;
		const decoded = await decryptLine(privateKey, prepared.body.ciphertext);
		assert.equal(decoded.email, email);
		assert.equal(decoded.use_case, useCase);
		assert.equal(includesBytes(base64ToBytes(prepared.body.ciphertext), new TextEncoder().encode(email)), false);
	}
});

/**
 * @param {string} body
 */
function fakeRequest(body) {
	const bytes = new TextEncoder().encode(body);
	const headers = new Map([
		['origin', 'https://tomiok.github.io'],
		['content-type', 'application/json'],
		['cf-connecting-ip', '203.0.113.9'],
	]);
	return {
		method: 'POST',
		headers: { get: (name) => headers.get(name.toLowerCase()) ?? null },
		body: {
			getReader() {
				let sent = false;
				return {
					async read() {
						if (sent) return { done: true, value: undefined };
						sent = true;
						return { done: false, value: bytes };
					},
					async cancel() {},
				};
			},
		},
	};
}
