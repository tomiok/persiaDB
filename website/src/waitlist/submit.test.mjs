import assert from 'node:assert/strict';
import test from 'node:test';

import { PayloadError } from './protocol/payload.mjs';
import { prepareSubmission } from './submit.mjs';

test('a filled honeypot does not need a public key', async () => {
	const result = await prepareSubmission({
		publicKeyPem: 'not-a-key',
		fields: { email: 'a@b.co', hp: 'https://spam.example' },
	});
	assert.deepEqual(result, { status: 'spam' });
});

test('a bad email fails before encryption', async () => {
	await assert.rejects(
		() => prepareSubmission({ publicKeyPem: 'not-a-key', fields: { email: 'nope', hp: '' } }),
		(error) => error instanceof PayloadError && error.code === 'email',
	);
});
