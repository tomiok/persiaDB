import { FIELD_LIMITS, MAX_PLAINTEXT_BYTES } from './constants.mjs';

const EMAIL =
	/^[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$/;

const CONTROL = /[\u0000-\u001F\u007F]/;
/** Use-case text may contain newlines. Other controls stay rejected. */
const CONTROL_EXCEPT_NEWLINE = /[\u0000-\u0009\u000B-\u001F\u007F]/;

export class PayloadError extends Error {
	/**
	 * @param {'email' | 'name' | 'company' | 'use_case' | 'too_large'} code
	 */
	constructor(code) {
		super(code);
		this.name = 'PayloadError';
		this.code = code;
	}
}

/**
 * @param {unknown} value
 * @param {'name' | 'company' | 'use_case'} code
 * @param {number} maxChars
 * @param {boolean} allowNewline
 */
function optionalText(value, code, maxChars, allowNewline) {
	if (value === undefined || value === null) return '';
	if (typeof value !== 'string') throw new PayloadError(code);
	const trimmed = value.trim();
	if (trimmed.length > maxChars) throw new PayloadError(code);
	const pattern = allowNewline ? CONTROL_EXCEPT_NEWLINE : CONTROL;
	if (pattern.test(trimmed)) throw new PayloadError(code);
	return trimmed;
}

/**
 * @param {{ email: unknown, name?: unknown, company?: unknown, useCase?: unknown, use_case?: unknown, now?: Date }} fields
 */
export function validateSubmission(fields) {
	if (typeof fields.email !== 'string') throw new PayloadError('email');
	const email = fields.email.trim().toLowerCase();
	if (email.length === 0 || email.length > FIELD_LIMITS.email || !EMAIL.test(email)) {
		throw new PayloadError('email');
	}
	const name = optionalText(fields.name, 'name', FIELD_LIMITS.name, false);
	const company = optionalText(fields.company, 'company', FIELD_LIMITS.company, false);
	const useCase = optionalText(
		fields.useCase !== undefined ? fields.useCase : fields.use_case,
		'use_case',
		FIELD_LIMITS.useCase,
		true,
	);
	const now = fields.now instanceof Date ? fields.now : new Date();
	let submittedAt;
	try {
		submittedAt = now.toISOString();
	} catch {
		throw new PayloadError('too_large');
	}
	const payload = {
		v: 1,
		email,
		name,
		company,
		use_case: useCase,
		submitted_at: submittedAt,
	};
	if (new TextEncoder().encode(JSON.stringify(payload)).length > MAX_PLAINTEXT_BYTES) {
		throw new PayloadError('too_large');
	}
	return payload;
}
