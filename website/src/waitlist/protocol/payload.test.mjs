import assert from 'node:assert/strict';
import test from 'node:test';

import { FIELD_LIMITS } from './constants.mjs';
import { PayloadError, validateSubmission } from './payload.mjs';

const now = new Date('2026-10-28T15:00:00.000Z');

test('accepts an email and keeps optional fields', () => {
	const payload = validateSubmission({
		email: '  Ada@Example.COM ',
		name: 'Ada',
		company: 'Analytical Engines',
		useCase: 'embedded search\nfor a notes app',
		now,
	});
	assert.deepEqual(payload, {
		v: 1,
		email: 'ada@example.com',
		name: 'Ada',
		company: 'Analytical Engines',
		use_case: 'embedded search\nfor a notes app',
		submitted_at: '2026-10-28T15:00:00.000Z',
	});
});

test('optional fields may be omitted', () => {
	const payload = validateSubmission({ email: 'a@b.co', now });
	assert.equal(payload.name, '');
	assert.equal(payload.company, '');
	assert.equal(payload.use_case, '');
});

test('rejects a bad email, controls, and overlong fields', () => {
	assert.throws(() => validateSubmission({ email: 'not-an-email', now }), PayloadError);
	assert.throws(() => validateSubmission({ email: 'a@b', now }), (error) => error.code === 'email');
	assert.throws(
		() => validateSubmission({ email: 'a@b.co', name: 'line\nbreak', now }),
		(error) => error.code === 'name',
	);
	assert.throws(
		() => validateSubmission({ email: 'a@b.co', use_case: 'x'.repeat(FIELD_LIMITS.useCase + 1), now }),
		(error) => error.code === 'use_case',
	);
	assert.throws(
		() => validateSubmission({ email: 'a@b.co', company: 'acme\u0000', now }),
		(error) => error.code === 'company',
	);
});
