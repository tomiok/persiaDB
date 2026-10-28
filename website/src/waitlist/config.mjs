/**
 * Relay URL baked into the static site.
 *
 * Leave this empty until the Cloudflare Worker is deployed, then put its
 * `https://….workers.dev` URL here (or set `PUBLIC_WAITLIST_RELAY_URL` when
 * `npm run build` runs). The URL is public. It is not a credential.
 */
export const defaultRelayUrl = '';

/**
 * Accept only an https URL, plus localhost for `astro dev`.
 * Anything else is treated as "not configured" so a bad value cannot be injected
 * into the page as a script URL.
 *
 * @param {unknown} value
 * @returns {string}
 */
export function sanitizeRelayUrl(value) {
	if (typeof value !== 'string') return '';
	const trimmed = value.trim();
	if (trimmed.length === 0 || trimmed.length > 200) return '';
	const https = /^https:\/\/[a-zA-Z0-9.-]+(?::\d{1,5})?(?:\/[A-Za-z0-9._~/-]*)?$/;
	const local = /^http:\/\/(?:localhost|127\.0\.0\.1)(?::\d{1,5})?(?:\/[A-Za-z0-9._~/-]*)?$/;
	if (https.test(trimmed) || local.test(trimmed)) return trimmed;
	return '';
}

/**
 * @param {string | undefined} fromEnv `PUBLIC_WAITLIST_RELAY_URL` at build time
 */
export function resolveRelayUrl(fromEnv) {
	const envUrl = sanitizeRelayUrl(fromEnv);
	if (envUrl) return envUrl;
	return sanitizeRelayUrl(defaultRelayUrl);
}
