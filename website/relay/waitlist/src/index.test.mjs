import assert from 'node:assert/strict';
import test from 'node:test';

import { WRAPPED_KEY_LEN } from '../../../src/waitlist/protocol/constants.mjs';
import { encodeLine, packEnvelope } from '../../../src/waitlist/protocol/envelope.mjs';
import {
	EVENT_TYPE,
	handleRequest,
	originAllowed,
	parseAllowedOrigins,
	resetMemoryLimiterForTests,
} from './index.mjs';

function line() {
	return encodeLine(
		packEnvelope({
			wrappedKey: new Uint8Array(WRAPPED_KEY_LEN).fill(8),
			iv: new Uint8Array(12).fill(1),
			ciphertext: new Uint8Array(20).fill(2),
		}),
	);
}

/**
 * @param {{ method?: string, headers?: Record<string, string>, body?: string }} [init]
 */
function request(init = {}) {
	const headers = new Map(
		Object.entries({
			origin: 'https://tomiok.github.io',
			'content-type': 'application/json',
			'cf-connecting-ip': '203.0.113.8',
			...init.headers,
		}).map(([key, value]) => [key.toLowerCase(), value]),
	);
	const bytes = new TextEncoder().encode(init.body ?? '');
	return {
		method: init.method ?? 'POST',
		headers: { get: (name) => headers.get(name.toLowerCase()) ?? null },
		body: {
			getReader() {
				let sent = false;
				return {
					async read() {
						if (sent) return { done: true, value: undefined };
						sent = true;
						if (bytes.length === 0) return { done: true, value: undefined };
						return { done: false, value: bytes };
					},
					async cancel() {},
				};
			},
		},
	};
}

function env(extra = {}) {
	return {
		ALLOWED_ORIGINS: 'https://tomiok.github.io,http://localhost:4321',
		GITHUB_REPOSITORY: 'tomiok/persiaDB',
		GITHUB_DISPATCH_TOKEN: 'github_pat_test_token_value_123456',
		WAITLIST_RATE_LIMIT: {
			async limit() {
				return { success: true };
			},
		},
		...extra,
	};
}

test('origin allowlist is exact', () => {
	const allowed = parseAllowedOrigins(' https://tomiok.github.io , http://localhost:4321 ');
	assert.equal(originAllowed('https://tomiok.github.io', allowed), true);
	assert.equal(originAllowed('https://tomiok.github.io.evil.test', allowed), false);
	assert.equal(originAllowed(null, allowed), false);
});

test('a valid ciphertext is dispatched once and the token stays out of the response', async () => {
	const ciphertext = line();
	/** @type {{ url: string, init: RequestInit }[]} */
	const calls = [];
	const response = await handleRequest(
		request({
			headers: { 'content-type': 'application/json; charset=utf-8' },
			body: JSON.stringify({ ciphertext, hp: '' }),
		}),
		env(),
		{
			async fetch(url, init) {
				calls.push({ url, init });
				return new Response(null, { status: 204 });
			},
		},
	);
	assert.equal(response.status, 200);
	const text = await response.text();
	assert.equal(text, '{"ok":true}');
	assert.equal(text.includes('github_pat'), false);
	assert.equal(calls.length, 1);
	assert.equal(calls[0].url, 'https://api.github.com/repos/tomiok/persiaDB/dispatches');
	const body = JSON.parse(String(calls[0].init.body));
	assert.equal(body.event_type, EVENT_TYPE);
	assert.deepEqual(body.client_payload, { ciphertext });
	assert.equal(calls[0].init.headers.authorization, 'Bearer github_pat_test_token_value_123456');
});

test('honeypot, bad ciphertext, extra fields, and a foreign origin do not dispatch', async () => {
	let calls = 0;
	const fetchImpl = async () => {
		calls += 1;
		return new Response(null, { status: 204 });
	};
	const honeypot = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: 'nope', hp: 'https://spam.test' }) }),
		env(),
		{ fetch: fetchImpl },
	);
	assert.equal(honeypot.status, 200);
	assert.equal(calls, 0);

	const bad = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: 'not-valid', hp: '' }) }),
		env(),
		{ fetch: fetchImpl },
	);
	assert.equal(bad.status, 400);
	assert.equal(calls, 0);

	const extra = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: line(), email: 'a@b.co' }) }),
		env(),
		{ fetch: fetchImpl },
	);
	assert.equal(extra.status, 400);

	const origin = await handleRequest(
		request({
			headers: { origin: 'https://evil.test' },
			body: JSON.stringify({ ciphertext: line(), hp: '' }),
		}),
		env(),
		{ fetch: fetchImpl },
	);
	assert.equal(origin.status, 403);
	assert.equal(origin.headers.get('access-control-allow-origin'), null);
	assert.equal(calls, 0);
});

test('preflight echoes only an allowed origin', async () => {
	const ok = await handleRequest(
		request({ method: 'OPTIONS', body: '' }),
		env(),
	);
	assert.equal(ok.status, 204);
	assert.equal(ok.headers.get('access-control-allow-origin'), 'https://tomiok.github.io');
	assert.equal(ok.headers.get('access-control-allow-methods'), 'POST, OPTIONS');

	const denied = await handleRequest(
		request({ method: 'OPTIONS', headers: { origin: 'https://evil.test' }, body: '' }),
		env(),
	);
	assert.equal(denied.status, 403);
});

test('rate limit and an upstream failure', async () => {
	const limited = await handleRequest(request({ body: JSON.stringify({ ciphertext: line(), hp: '' }) }), env({
		WAITLIST_RATE_LIMIT: {
			async limit() {
				return { success: false };
			},
		},
	}));
	assert.equal(limited.status, 429);

	let calls = 0;
	const upstream = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: line(), hp: '' }) }),
		env(),
		{
			async fetch() {
				calls += 1;
				return new Response('no', { status: 401 });
			},
		},
	);
	assert.equal(upstream.status, 502);
	assert.equal(calls, 1);
	assert.equal((await upstream.text()).includes('github_pat'), false);
});

test('memory limiter allows five posts per minute for one address', async () => {
	resetMemoryLimiterForTests();
	let nowMs = 1_000_000;
	const localEnv = env({ WAITLIST_RATE_LIMIT: undefined });
	let calls = 0;
	const fetchImpl = async () => {
		calls += 1;
		return new Response(null, { status: 204 });
	};
	for (let i = 0; i < 5; i++) {
		const response = await handleRequest(
			request({ body: JSON.stringify({ ciphertext: line(), hp: '' }) }),
			localEnv,
			{ fetch: fetchImpl, now: () => nowMs },
		);
		assert.equal(response.status, 200);
	}
	const blocked = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: line(), hp: '' }) }),
		localEnv,
		{ fetch: fetchImpl, now: () => nowMs },
	);
	assert.equal(blocked.status, 429);
	assert.equal(calls, 5);
	nowMs += 60_000;
	const again = await handleRequest(
		request({ body: JSON.stringify({ ciphertext: line(), hp: '' }) }),
		localEnv,
		{ fetch: fetchImpl, now: () => nowMs },
	);
	assert.equal(again.status, 200);
	resetMemoryLimiterForTests();
});

test('oversized declared body is rejected before dispatch', async () => {
	let calls = 0;
	const response = await handleRequest(
		request({
			headers: { 'content-length': '999999' },
			body: JSON.stringify({ ciphertext: line(), hp: '' }),
		}),
		env(),
		{
			async fetch() {
				calls += 1;
				return new Response(null, { status: 204 });
			},
		},
	);
	assert.equal(response.status, 413);
	assert.equal(calls, 0);
});
