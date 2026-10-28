#!/usr/bin/env node
// Validate WAITLIST_CIPHERTEXT and append it to a ciphertext file.
// Exit 0 when the line was added, 2 when it was already present, 1 when invalid.
// The GitHub Action runs this on the default branch, then commits the result
// onto the waitlist branch. It does not decrypt.

import { readFileSync, writeFileSync } from 'node:fs';

import { mergeCiphertextFile } from '../../website/src/waitlist/protocol/store.mjs';

/**
 * @param {string} flag
 */
function arg(flag) {
	const index = process.argv.indexOf(flag);
	if (index === -1 || !process.argv[index + 1]) {
		console.error(`missing ${flag}`);
		process.exit(1);
	}
	return process.argv[index + 1];
}

const currentPath = arg('--current');
const outPath = arg('--out');
const ciphertext = process.env.WAITLIST_CIPHERTEXT;
if (typeof ciphertext !== 'string' || ciphertext.trim().length === 0) {
	console.error('invalid waitlist ciphertext');
	process.exit(1);
}

let existing = '';
try {
	existing = readFileSync(currentPath, 'utf8');
} catch (error) {
	if (!error || typeof error !== 'object' || error.code !== 'ENOENT') throw error;
}

try {
	const result = mergeCiphertextFile(existing, ciphertext.trim());
	if (!result.changed) process.exit(2);
	writeFileSync(outPath, result.text, { mode: 0o600 });
} catch {
	console.error('invalid waitlist ciphertext');
	process.exit(1);
}
