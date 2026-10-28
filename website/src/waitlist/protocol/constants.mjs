// Waitlist envelope limits. This is not the Persia DB on-disk format.
// A line in waitlist.txt.enc is base64 of a PWLE envelope: an AES-256-GCM
// payload whose key is wrapped with RSA-OAEP (SHA-256, 4096-bit).

/** ASCII "PWLE" (Persia Waitlist Line Envelope). */
export const MAGIC = new Uint8Array([0x50, 0x57, 0x4c, 0x45]);

export const VERSION = 1;

/** RSA-OAEP ciphertext for a 4096-bit key. keygen refuses any other size. */
export const WRAPPED_KEY_LEN = 512;

export const IV_LEN = 12;

/** AES-GCM authentication tag, appended to the ciphertext by WebCrypto. */
export const TAG_LEN = 16;

export const HEADER_LEN = 7;

/** UTF-8 plaintext cap. Field limits are tighter; this is the hard ceiling. */
export const MAX_PLAINTEXT_BYTES = 2048;

export const MAX_ENVELOPE_BYTES =
	HEADER_LEN + WRAPPED_KEY_LEN + IV_LEN + MAX_PLAINTEXT_BYTES + TAG_LEN;

/**
 * Base64 of the largest envelope is 3460 characters. 4096 leaves a little
 * room and is the size the worker and the Action both reject past.
 */
export const MAX_LINE_CHARS = 4096;

/** JSON body cap at the relay, including the line and the honeypot field. */
export const MAX_BODY_BYTES = 8192;

/** Bound into AES-GCM so a blob cannot be replayed under a different label. */
export const AAD_LABEL = 'persia-waitlist-v1';

export const FIELD_LIMITS = {
	email: 254,
	name: 120,
	company: 120,
	useCase: 500,
};
