import assert from 'node:assert/strict';
import test from 'node:test';

import { base64ToBytes, bytesToBase64 } from './bytes.mjs';
import { MAX_LINE_CHARS, WRAPPED_KEY_LEN } from './constants.mjs';
import { assertCiphertextLine, encodeLine, packEnvelope, unpackEnvelope } from './envelope.mjs';

/** Structurally valid envelope. The bytes are not a real RSA wrapping. */
function sample() {
	return packEnvelope({
		wrappedKey: new Uint8Array(WRAPPED_KEY_LEN).fill(4),
		iv: new Uint8Array(12).fill(5),
		ciphertext: new Uint8Array(16).fill(6),
	});
}

test('envelope round-trips and encodes one canonical line', () => {
	const bytes = sample();
	const parts = unpackEnvelope(bytes);
	assert.equal(parts.wrappedKey.length, WRAPPED_KEY_LEN);
	assert.equal(parts.iv.length, 12);
	assert.equal(parts.ciphertext.length, 16);
	const line = encodeLine(bytes);
	assert.equal(assertCiphertextLine(line), line);
	assert.equal(line.includes('\n'), false);
	assert.ok(line.length <= MAX_LINE_CHARS);
	assert.deepEqual(base64ToBytes(line), bytes);
});

test('rejects a short buffer, a bad magic, a bad version, and a wrong key length', () => {
	assert.throws(() => unpackEnvelope(new Uint8Array(10)), /envelope length/);
	const bytes = sample();
	bytes[0] = 0;
	assert.throws(() => unpackEnvelope(bytes), /magic/);
	const version = sample();
	version[4] = 9;
	assert.throws(() => unpackEnvelope(version), /version/);
	const keyLen = sample();
	keyLen[5] = 32;
	keyLen[6] = 0;
	assert.throws(() => unpackEnvelope(keyLen), /wrapped key/);
});

test('rejects non-canonical and non-base64 lines', () => {
	const line = encodeLine(sample());
	assert.throws(() => assertCiphertextLine(`${line}\n`), /invalid ciphertext/);
	assert.throws(() => assertCiphertextLine(`-${line.slice(1)}`), /invalid ciphertext/);
	assert.throws(() => assertCiphertextLine(''), /invalid ciphertext/);
	assert.throws(() => assertCiphertextLine('@@@@'), /invalid ciphertext/);
	assert.throws(() => assertCiphertextLine('A'.repeat(MAX_LINE_CHARS + 1)), /invalid ciphertext/);
	const decoded = bytesToBase64(new Uint8Array([1, 2, 3]));
	assert.throws(() => assertCiphertextLine(decoded), /invalid ciphertext/);
});
