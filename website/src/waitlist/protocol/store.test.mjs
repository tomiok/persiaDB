import assert from 'node:assert/strict';
import test from 'node:test';

import { WRAPPED_KEY_LEN } from './constants.mjs';
import { encodeLine, packEnvelope } from './envelope.mjs';
import { mergeCiphertextFile } from './store.mjs';

function line(byte) {
	return encodeLine(
		packEnvelope({
			wrappedKey: new Uint8Array(WRAPPED_KEY_LEN).fill(byte),
			iv: new Uint8Array(12).fill(1),
			ciphertext: new Uint8Array(16).fill(2),
		}),
	);
}

test('appends a line, skips a duplicate, and keeps comments', () => {
	const first = line(3);
	const second = line(4);
	const initial = mergeCiphertextFile('# kept\n', first);
	assert.equal(initial.changed, true);
	assert.equal(initial.text, `# kept\n${first}\n`);
	const again = mergeCiphertextFile(initial.text, first);
	assert.equal(again.changed, false);
	assert.equal(again.text, initial.text);
	const next = mergeCiphertextFile(initial.text.replace(/\n$/, ''), second);
	assert.equal(next.changed, true);
	assert.ok(next.text.endsWith(`${second}\n`));
	assert.throws(() => mergeCiphertextFile('', 'not-ciphertext'), /invalid ciphertext/);
});
