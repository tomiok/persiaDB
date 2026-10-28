/** @param {Uint8Array} bytes */
export function bytesToBase64(bytes) {
	const chunk = 0x1000;
	let binary = '';
	for (let offset = 0; offset < bytes.length; offset += chunk) {
		const slice = bytes.subarray(offset, offset + chunk);
		binary += String.fromCharCode.apply(null, slice);
	}
	return btoa(binary);
}

/**
 * Strict standard base64. Rejects base64url, whitespace, and missing padding.
 * @param {string} text
 * @returns {Uint8Array}
 */
export function base64ToBytes(text) {
	if (typeof text !== 'string' || !/^[A-Za-z0-9+/]+={0,2}$/.test(text) || text.length % 4 !== 0) {
		throw new Error('invalid base64');
	}
	let binary;
	try {
		binary = atob(text);
	} catch {
		throw new Error('invalid base64');
	}
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	return bytes;
}

/**
 * @param {string} pem
 * @param {string} label `PUBLIC KEY` or `PRIVATE KEY`
 * @returns {ArrayBuffer}
 */
export function pemToDer(pem, label) {
	if (typeof pem !== 'string') throw new Error('pem');
	const begin = `-----BEGIN ${label}-----`;
	const end = `-----END ${label}-----`;
	const start = pem.indexOf(begin);
	const stop = pem.indexOf(end);
	if (start === -1 || stop <= start) throw new Error('pem');
	const body = pem.slice(start + begin.length, stop).replace(/[^A-Za-z0-9+/=]/g, '');
	const bytes = base64ToBytes(body);
	return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
}
