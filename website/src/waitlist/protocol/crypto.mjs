// Hybrid encryption that browsers and Node both implement with WebCrypto.
// No age or libsodium dependency: the static site cannot ship a native module,
// and the decrypt script should run with the Node that already builds the site.
//
// RSA-4096 OAEP-SHA-256 wraps a fresh AES-256-GCM key. Only the matching
// private key can unwrap it. The private key is never an input to the encrypt
// path and must not be committed.

import { base64ToBytes, pemToDer } from './bytes.mjs';
import { AAD_LABEL, IV_LEN, MAX_PLAINTEXT_BYTES, WRAPPED_KEY_LEN } from './constants.mjs';
import { assertCiphertextLine, encodeLine, packEnvelope, unpackEnvelope } from './envelope.mjs';

const RSA_OAEP = { name: 'RSA-OAEP', hash: 'SHA-256' };
const AAD = standalone(new TextEncoder().encode(AAD_LABEL));

/** WebCrypto uses the underlying buffer, so views must be copied first. */
function standalone(bytes) {
	const copy = new Uint8Array(bytes.length);
	copy.set(bytes);
	return copy;
}

/** @param {string} text */
export function looksLikeSpkiPem(text) {
	return (
		typeof text === 'string' &&
		text.includes('-----BEGIN PUBLIC KEY-----') &&
		text.includes('-----END PUBLIC KEY-----')
	);
}

/** @param {string} pem */
export async function importPublicKey(pem) {
	const der = pemToDer(pem, 'PUBLIC KEY');
	return crypto.subtle.importKey('spki', der, RSA_OAEP, false, ['encrypt']);
}

/** @param {string} pem */
export async function importPrivateKey(pem) {
	const der = pemToDer(pem, 'PRIVATE KEY');
	return crypto.subtle.importKey('pkcs8', der, RSA_OAEP, false, ['decrypt']);
}

/**
 * @param {CryptoKey} publicKey
 * @param {Uint8Array} plaintext
 */
export async function encryptBytes(publicKey, plaintext) {
	if (!(plaintext instanceof Uint8Array) || plaintext.length > MAX_PLAINTEXT_BYTES) {
		throw new Error('plaintext length');
	}
	const aes = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt']);
	const iv = crypto.getRandomValues(new Uint8Array(IV_LEN));
	const ciphertext = new Uint8Array(
		await crypto.subtle.encrypt(
			{ name: 'AES-GCM', iv, additionalData: AAD },
			aes,
			standalone(plaintext),
		),
	);
	const rawAes = new Uint8Array(await crypto.subtle.exportKey('raw', aes));
	try {
		const wrapped = new Uint8Array(
			await crypto.subtle.encrypt({ name: 'RSA-OAEP' }, publicKey, standalone(rawAes)),
		);
		if (wrapped.length !== WRAPPED_KEY_LEN) throw new Error('wrapped key');
		return packEnvelope({ wrappedKey: wrapped, iv, ciphertext });
	} finally {
		rawAes.fill(0);
	}
}

/**
 * @param {CryptoKey} privateKey
 * @param {Uint8Array} bytes
 */
export async function decryptBytes(privateKey, bytes) {
	const { wrappedKey, iv, ciphertext } = unpackEnvelope(bytes);
	const rawAes = new Uint8Array(
		await crypto.subtle.decrypt({ name: 'RSA-OAEP' }, privateKey, standalone(wrappedKey)),
	);
	try {
		if (rawAes.length !== 32) throw new Error('unwrapped key');
		const aes = await crypto.subtle.importKey('raw', standalone(rawAes), { name: 'AES-GCM' }, false, [
			'decrypt',
		]);
		return new Uint8Array(
			await crypto.subtle.decrypt(
				{ name: 'AES-GCM', iv: standalone(iv), additionalData: AAD },
				aes,
				standalone(ciphertext),
			),
		);
	} finally {
		rawAes.fill(0);
	}
}

/**
 * @param {string} publicPem
 * @param {unknown} payload JSON-serializable object already validated
 */
export async function encryptSubmission(publicPem, payload) {
	const plaintext = new TextEncoder().encode(JSON.stringify(payload));
	const publicKey = await importPublicKey(publicPem);
	return encodeLine(await encryptBytes(publicKey, plaintext));
}

/**
 * @param {string} privatePem
 * @param {string} line
 */
export async function decryptLine(privatePem, line) {
	const canonical = assertCiphertextLine(line);
	const privateKey = await importPrivateKey(privatePem);
	const plain = await decryptBytes(privateKey, base64ToBytes(canonical));
	let text;
	try {
		text = new TextDecoder('utf-8', { fatal: true }).decode(plain);
	} catch {
		throw new Error('payload');
	}
	const value = JSON.parse(text);
	if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('payload');
	return value;
}
