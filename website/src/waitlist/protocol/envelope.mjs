import { base64ToBytes, bytesToBase64 } from './bytes.mjs';
import {
	HEADER_LEN,
	IV_LEN,
	MAGIC,
	MAX_ENVELOPE_BYTES,
	MAX_LINE_CHARS,
	MAX_PLAINTEXT_BYTES,
	TAG_LEN,
	VERSION,
	WRAPPED_KEY_LEN,
} from './constants.mjs';

/**
 * @param {{ wrappedKey: Uint8Array, iv: Uint8Array, ciphertext: Uint8Array }} parts
 * @returns {Uint8Array}
 */
export function packEnvelope(parts) {
	const { wrappedKey, iv, ciphertext } = parts;
	if (!(wrappedKey instanceof Uint8Array) || wrappedKey.length !== WRAPPED_KEY_LEN) {
		throw new Error('wrapped key');
	}
	if (!(iv instanceof Uint8Array) || iv.length !== IV_LEN) throw new Error('iv');
	if (!(ciphertext instanceof Uint8Array) || ciphertext.length < TAG_LEN) throw new Error('ciphertext');
	if (ciphertext.length > MAX_PLAINTEXT_BYTES + TAG_LEN) throw new Error('ciphertext length');
	const total = HEADER_LEN + wrappedKey.length + iv.length + ciphertext.length;
	if (total > MAX_ENVELOPE_BYTES) throw new Error('envelope length');
	const out = new Uint8Array(total);
	out.set(MAGIC, 0);
	out[4] = VERSION;
	out[5] = WRAPPED_KEY_LEN & 0xff;
	out[6] = (WRAPPED_KEY_LEN >> 8) & 0xff;
	let offset = HEADER_LEN;
	out.set(wrappedKey, offset);
	offset += wrappedKey.length;
	out.set(iv, offset);
	offset += iv.length;
	out.set(ciphertext, offset);
	return out;
}

/**
 * @param {Uint8Array} bytes
 * @returns {{ wrappedKey: Uint8Array, iv: Uint8Array, ciphertext: Uint8Array }}
 */
export function unpackEnvelope(bytes) {
	if (!(bytes instanceof Uint8Array) || bytes.length > MAX_ENVELOPE_BYTES) {
		throw new Error('envelope length');
	}
	if (bytes.length < HEADER_LEN + WRAPPED_KEY_LEN + IV_LEN + TAG_LEN) {
		throw new Error('envelope length');
	}
	for (let i = 0; i < MAGIC.length; i++) {
		if (bytes[i] !== MAGIC[i]) throw new Error('magic');
	}
	if (bytes[4] !== VERSION) throw new Error('version');
	const wrappedLen = bytes[5] | (bytes[6] << 8);
	if (wrappedLen !== WRAPPED_KEY_LEN) throw new Error('wrapped key');
	let offset = HEADER_LEN;
	const wrappedKey = bytes.subarray(offset, offset + WRAPPED_KEY_LEN);
	offset += WRAPPED_KEY_LEN;
	const iv = bytes.subarray(offset, offset + IV_LEN);
	offset += IV_LEN;
	const ciphertext = bytes.subarray(offset);
	if (ciphertext.length < TAG_LEN || ciphertext.length > MAX_PLAINTEXT_BYTES + TAG_LEN) {
		throw new Error('ciphertext length');
	}
	return { wrappedKey, iv, ciphertext };
}

/** @param {Uint8Array} bytes */
export function encodeLine(bytes) {
	const packed = unpackEnvelope(bytes);
	if (packed.wrappedKey.length !== WRAPPED_KEY_LEN) throw new Error('wrapped key');
	const line = bytesToBase64(bytes);
	if (line.length > MAX_LINE_CHARS) throw new Error('line length');
	return line;
}

/**
 * One canonical base64 line, or a throw. Callers that talk to strangers
 * (the relay, the Action) use this and discard the internal reason.
 * @param {string} line
 */
export function assertCiphertextLine(line) {
	if (typeof line !== 'string' || line.length === 0 || line.length > MAX_LINE_CHARS) {
		throw new Error('invalid ciphertext');
	}
	let bytes;
	try {
		bytes = base64ToBytes(line);
	} catch {
		throw new Error('invalid ciphertext');
	}
	if (bytesToBase64(bytes) !== line) throw new Error('invalid ciphertext');
	if (bytes.length > MAX_ENVELOPE_BYTES) throw new Error('invalid ciphertext');
	try {
		unpackEnvelope(bytes);
	} catch {
		throw new Error('invalid ciphertext');
	}
	return line;
}
