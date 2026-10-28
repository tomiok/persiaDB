// Relay for the static waitlist form.
// The browser already encrypted the submission. This worker checks the shape,
// rate-limits, and asks GitHub to append the line. It never sees plaintext and
// it never embeds the token in a response.

import { MAX_BODY_BYTES, MAX_LINE_CHARS } from '../../../src/waitlist/protocol/constants.mjs';
import { assertCiphertextLine } from '../../../src/waitlist/protocol/envelope.mjs';

export const EVENT_TYPE = 'waitlist-submission';

const memoryBuckets = new Map();

/** @internal test hook */
export function resetMemoryLimiterForTests() {
	memoryBuckets.clear();
}

export class WaitlistError extends Error {
	/**
	 * @param {number} status
	 * @param {string} code
	 */
	constructor(status, code) {
		super(code);
		this.name = 'WaitlistError';
		this.status = status;
		this.code = code;
	}
}

/**
 * @param {string | undefined} value
 * @returns {string[]}
 */
export function parseAllowedOrigins(value) {
	if (typeof value !== 'string') return [];
	return value
		.split(',')
		.map((part) => part.trim())
		.filter((part) => part.length > 0);
}

/**
 * @param {string | null} origin
 * @param {readonly string[]} allowed
 */
export function originAllowed(origin, allowed) {
	return typeof origin === 'string' && allowed.includes(origin);
}

/**
 * @param {{ method: string, headers: { get(name: string): string | null }, body?: { getReader(): ReadableStreamDefaultReader<Uint8Array> } | null }} request
 * @param {Record<string, any>} env
 * @param {{ fetch?: typeof fetch, now?: () => number }} [deps]
 */
export async function handleRequest(request, env, deps = {}) {
	const fetchImpl = deps.fetch ?? fetch;
	const now = deps.now ?? Date.now;
	try {
		if (request.method === 'OPTIONS') return preflight(request, env);
		if (request.method !== 'POST') {
			return respond(request, env, 405, { ok: false, error: 'method' }, 'POST, OPTIONS');
		}
		const allowed = parseAllowedOrigins(env.ALLOWED_ORIGINS);
		const origin = request.headers.get('Origin');
		if (!originAllowed(origin, allowed)) {
			return respond(request, env, 403, { ok: false, error: 'origin' });
		}
		if (!isJson(request.headers.get('content-type'))) {
			return respond(request, env, 415, { ok: false, error: 'type' });
		}
		if (await rateLimited(request, env, now)) {
			return respond(request, env, 429, { ok: false, error: 'slow_down' });
		}
		const raw = await readLimited(request, MAX_BODY_BYTES);
		let body;
		try {
			body = JSON.parse(raw);
		} catch {
			throw new WaitlistError(400, 'invalid');
		}
		if (!body || typeof body !== 'object' || Array.isArray(body)) {
			throw new WaitlistError(400, 'invalid');
		}
		if (honeypotTripped(body.hp)) return respond(request, env, 200, { ok: true });
		for (const key of Object.keys(body)) {
			if (key !== 'ciphertext' && key !== 'hp') throw new WaitlistError(400, 'invalid');
		}
		if (typeof body.ciphertext !== 'string') throw new WaitlistError(400, 'invalid');
		const trimmed = body.ciphertext.trim();
		if (trimmed.length > MAX_LINE_CHARS) throw new WaitlistError(413, 'too_large');
		let line;
		try {
			line = assertCiphertextLine(trimmed);
		} catch {
			throw new WaitlistError(400, 'invalid');
		}
		await dispatch(env, line, fetchImpl);
		return respond(request, env, 200, { ok: true });
	} catch (error) {
		if (error instanceof WaitlistError) {
			return respond(request, env, error.status, { ok: false, error: error.code });
		}
		console.log('waitlist failed');
		return respond(request, env, 500, { ok: false, error: 'error' });
	}
}

export default {
	/** @param {Request} request @param {Record<string, any>} env */
	fetch(request, env) {
		return handleRequest(request, env);
	},
};

/**
 * @param {any} request
 * @param {Record<string, any>} env
 * @param {number} status
 * @param {Record<string, unknown>} body
 * @param {string} [allow]
 */
function respond(request, env, status, body, allow) {
	const headers = corsHeaders(request, env);
	headers.set('content-type', 'application/json; charset=utf-8');
	if (allow) headers.set('allow', allow);
	return new Response(JSON.stringify(body), { status, headers });
}

/**
 * @param {any} request
 * @param {Record<string, any>} env
 */
function preflight(request, env) {
	const allowed = parseAllowedOrigins(env.ALLOWED_ORIGINS);
	if (!originAllowed(request.headers.get('Origin'), allowed)) {
		return respond(request, env, 403, { ok: false, error: 'origin' });
	}
	const headers = corsHeaders(request, env);
	headers.set('access-control-allow-methods', 'POST, OPTIONS');
	headers.set('access-control-allow-headers', 'content-type');
	headers.set('access-control-max-age', '86400');
	return new Response(null, { status: 204, headers });
}

/**
 * @param {any} request
 * @param {Record<string, any>} env
 */
function corsHeaders(request, env) {
	const headers = new Headers();
	headers.set('vary', 'Origin');
	headers.set('cache-control', 'no-store');
	const origin = request.headers.get('Origin');
	if (originAllowed(origin, parseAllowedOrigins(env.ALLOWED_ORIGINS))) {
		headers.set('access-control-allow-origin', origin);
	}
	return headers;
}

/**
 * @param {string | null} value
 */
function isJson(value) {
	if (!value) return false;
	const type = value.split(';', 1)[0];
	return type.trim().toLowerCase() === 'application/json';
}

/**
 * @param {unknown} value
 */
function honeypotTripped(value) {
	if (value === undefined || value === null) return false;
	if (typeof value === 'string') return value.trim() !== '';
	return true;
}

/**
 * @param {any} request
 */
function clientKey(request) {
	const raw = request.headers.get('CF-Connecting-IP') ?? '';
	if (/^[A-Za-z0-9:.]{1,64}$/.test(raw)) return raw;
	return 'unknown';
}

/**
 * @param {any} request
 * @param {Record<string, any>} env
 * @param {() => number} now
 */
async function rateLimited(request, env, now) {
	const key = clientKey(request);
	const limiter = env.WAITLIST_RATE_LIMIT;
	if (limiter && typeof limiter.limit === 'function') {
		const result = await limiter.limit({ key });
		return !result || result.success !== true;
	}
	return memoryLimited(key, now());
}

/**
 * Best-effort limiter for `wrangler dev` when the binding is absent.
 * Production uses the rate-limit binding in wrangler.toml. Isolates do not
 * share this Map, so it is not the production control.
 *
 * @param {string} key
 * @param {number} nowMs
 */
function memoryLimited(key, nowMs) {
	const windowMs = 60_000;
	const limit = 5;
	const bucket = memoryBuckets.get(key);
	if (!bucket || nowMs - bucket.start >= windowMs) {
		memoryBuckets.set(key, { start: nowMs, count: 1 });
		return false;
	}
	bucket.count += 1;
	return bucket.count > limit;
}

/**
 * @param {any} request
 * @param {number} max
 */
async function readLimited(request, max) {
	const declared = request.headers.get('content-length');
	if (declared !== null) {
		if (!/^\d+$/.test(declared) || Number(declared) > max) throw new WaitlistError(413, 'too_large');
	}
	if (!request.body || typeof request.body.getReader !== 'function') return '';
	const reader = request.body.getReader();
	/** @type {Uint8Array[]} */
	const chunks = [];
	let total = 0;
	for (;;) {
		const { done, value } = await reader.read();
		if (done) break;
		if (!(value instanceof Uint8Array)) throw new WaitlistError(400, 'invalid');
		total += value.byteLength;
		if (total > max) {
			if (typeof reader.cancel === 'function') await reader.cancel();
			throw new WaitlistError(413, 'too_large');
		}
		chunks.push(value);
	}
	const buf = new Uint8Array(total);
	let offset = 0;
	for (const chunk of chunks) {
		buf.set(chunk, offset);
		offset += chunk.byteLength;
	}
	try {
		return new TextDecoder('utf-8', { fatal: true }).decode(buf);
	} catch {
		throw new WaitlistError(400, 'invalid');
	}
}

/**
 * @param {Record<string, any>} env
 * @param {string} line
 * @param {typeof fetch} fetchImpl
 */
async function dispatch(env, line, fetchImpl) {
	const repo = env.GITHUB_REPOSITORY;
	const token = env.GITHUB_DISPATCH_TOKEN;
	if (typeof repo !== 'string' || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
		throw new WaitlistError(500, 'misconfigured');
	}
	if (typeof token !== 'string' || token.length < 20 || token.length > 300 || /\s/.test(token)) {
		throw new WaitlistError(500, 'misconfigured');
	}
	let response;
	try {
		response = await fetchImpl(`https://api.github.com/repos/${repo}/dispatches`, {
			method: 'POST',
			headers: {
				authorization: `Bearer ${token}`,
				accept: 'application/vnd.github+json',
				'content-type': 'application/json',
				'user-agent': 'persia-waitlist',
				'x-github-api-version': '2022-11-28',
			},
			body: JSON.stringify({
				event_type: EVENT_TYPE,
				client_payload: { ciphertext: line },
			}),
			signal: AbortSignal.timeout(10_000),
		});
	} catch {
		throw new WaitlistError(502, 'upstream');
	}
	if (response.status !== 204 && response.status !== 200) {
		console.log(`waitlist dispatch status ${response.status}`);
		throw new WaitlistError(502, 'upstream');
	}
}
