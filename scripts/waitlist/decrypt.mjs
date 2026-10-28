#!/usr/bin/env node
// Decrypt waitlist.txt.enc with the private key that keygen wrote.
// The key is read from a path you pass. Nothing in this script contacts the network.

import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

import { decryptLine } from '../../website/src/waitlist/protocol/crypto.mjs';

/**
 * @param {string} name
 */
function arg(name) {
	const index = process.argv.indexOf(name);
	if (index === -1) return undefined;
	const value = process.argv[index + 1];
	if (!value || value.startsWith('--')) return undefined;
	return value;
}

const keyPath = arg('--key') ?? process.env.PERSIA_WAITLIST_PRIVATE_KEY;
const file = arg('--file');
const fromGit = arg('--from-git');

if (!keyPath || (file && fromGit) || (!file && !fromGit)) {
	console.error(
		'usage: node scripts/waitlist/decrypt.mjs --key <private.pem> (--file <waitlist.txt.enc> | --from-git <ref>)',
	);
	process.exit(2);
}

/** @type {string} */
let text;
if (fromGit) {
	if (!/^[A-Za-z0-9._/]+$/.test(fromGit)) {
		console.error('refusing ref');
		process.exit(2);
	}
	const show = spawnSync('git', ['show', `${fromGit}:waitlist.txt.enc`], { encoding: 'utf8' });
	if (show.status !== 0) {
		process.stderr.write(show.stderr || 'git show failed\n');
		process.exit(1);
	}
	text = show.stdout;
} else {
	text = readFileSync(file, 'utf8');
}

const pem = readFileSync(keyPath, 'utf8');
if (pem.includes('BEGIN PUBLIC KEY') && !pem.includes('BEGIN PRIVATE KEY')) {
	console.error('that file is a public key; pass the private key');
	process.exit(2);
}

const lines = text.split(/\r?\n/);
let failed = 0;
for (let i = 0; i < lines.length; i++) {
	const line = lines[i];
	if (line.length === 0 || line.startsWith('#')) continue;
	try {
		const value = await decryptLine(pem, line);
		process.stdout.write(`${JSON.stringify(value)}\n`);
	} catch {
		failed += 1;
		console.error(`line ${i + 1}: could not decrypt`);
	}
}
process.exit(failed === 0 ? 0 : 1);
