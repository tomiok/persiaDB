import { assertCiphertextLine } from './envelope.mjs';

/**
 * Append one canonical ciphertext line. Blank lines and `#` comments in the
 * existing file are preserved. A repeated line is not appended again.
 *
 * @param {string} existing
 * @param {string} line
 * @returns {{ changed: boolean, text: string }}
 */
export function mergeCiphertextFile(existing, line) {
	if (typeof existing !== 'string') throw new Error('invalid ciphertext');
	const canonical = assertCiphertextLine(line);
	const normalized = existing.replace(/\r\n/g, '\n');
	const already = normalized.split('\n').some((entry) => entry === canonical);
	const terminated =
		normalized.length === 0 || normalized.endsWith('\n') ? normalized : `${normalized}\n`;
	if (already) return { changed: false, text: terminated };
	return { changed: true, text: `${terminated}${canonical}\n` };
}
