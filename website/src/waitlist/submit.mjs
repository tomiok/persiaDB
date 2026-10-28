import { encryptSubmission } from './protocol/crypto.mjs';
import { PayloadError, validateSubmission } from './protocol/payload.mjs';

export { PayloadError };

/**
 * Turn form fields into the JSON body the relay accepts.
 * A filled honeypot reports `spam` and must not be sent. The UI still says
 * the note was sent, so the field does not teach a bot what worked.
 *
 * @param {{ publicKeyPem: string, fields: { email?: unknown, name?: unknown, company?: unknown, useCase?: unknown, use_case?: unknown, hp?: unknown }, now?: Date }} input
 * @returns {Promise<{ status: 'spam' } | { status: 'ready', body: { ciphertext: string, hp: string } }>}
 */
export async function prepareSubmission(input) {
	const hp = input.fields.hp;
	if (typeof hp === 'string' ? hp.trim() !== '' : hp !== undefined && hp !== null && hp !== '') {
		return { status: 'spam' };
	}
	const payload = validateSubmission({
		email: input.fields.email,
		name: input.fields.name,
		company: input.fields.company,
		useCase: input.fields.useCase !== undefined ? input.fields.useCase : input.fields.use_case,
		now: input.now,
	});
	const ciphertext = await encryptSubmission(input.publicKeyPem, payload);
	return { status: 'ready', body: { ciphertext, hp: '' } };
}
