#!/usr/bin/env node
// Generate the waitlist RSA-4096 keypair.
// The public key is written where the site can import it. The private key
// defaults to ~/.config/persia/ and this script refuses to place it inside
// the repository.

import { generateKeyPairSync } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));

/**
 * @param {string} start
 * @returns {string | null}
 */
export function findRepoRoot(start) {
	let dir = path.resolve(start);
	for (;;) {
		if (existsSync(path.join(dir, '.git'))) return dir;
		const parent = path.dirname(dir);
		if (parent === dir) return null;
		dir = parent;
	}
}

/**
 * @param {string} target
 * @param {string} repoRoot
 */
export function isInsideRepo(target, repoRoot) {
	const root = realpathSync(repoRoot);
	const resolved = path.resolve(target);
	const parent = path.dirname(resolved);
	const base = existsSync(parent) ? realpathSync(parent) : path.resolve(parent);
	const full = path.join(base, path.basename(resolved));
	const rel = path.relative(root, full);
	return rel === '' || (!rel.startsWith(`..${path.sep}`) && rel !== '..' && !path.isAbsolute(rel));
}

/**
 * @param {string} target
 * @param {string | null} repoRoot
 */
export function assertPrivateKeyDestination(target, repoRoot) {
	if (repoRoot && isInsideRepo(target, repoRoot)) {
		throw new Error(
			`refusing to write a private key inside the repository (${path.resolve(target)})`,
		);
	}
}

/** RSA-4096 SPKI + PKCS#8 PEMs. 4096 matches the envelope's 512-byte wrapped key. */
export function generateKeyPairPem() {
	const { publicKey, privateKey } = generateKeyPairSync('rsa', {
		modulusLength: 4096,
		publicExponent: 0x10001,
		publicKeyEncoding: { type: 'spki', format: 'pem' },
		privateKeyEncoding: { type: 'pkcs8', format: 'pem' },
	});
	return { publicKey, privateKey };
}

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

function main() {
	const repoRoot = findRepoRoot(here);
	const publicPath = path.resolve(
		arg('--public') ??
			(repoRoot
				? path.join(repoRoot, 'website/src/waitlist/public.pem')
				: path.join(here, 'public.pem')),
	);
	const privatePath = path.resolve(
		arg('--private') ?? path.join(os.homedir(), '.config', 'persia', 'waitlist-private.pem'),
	);
	const force = process.argv.includes('--force');
	assertPrivateKeyDestination(privatePath, repoRoot);
	if (path.resolve(privatePath) === path.resolve(publicPath)) {
		throw new Error('public and private paths must differ');
	}
	if (!force && existsSync(privatePath)) {
		throw new Error(`private key already exists (${privatePath}); pass --force to overwrite`);
	}
	if (!force && existsSync(publicPath)) {
		const current = readFileSync(publicPath, 'utf8');
		if (current.includes('-----BEGIN PUBLIC KEY-----')) {
			throw new Error(`public key already exists (${publicPath}); pass --force to overwrite`);
		}
	}
	const { publicKey, privateKey } = generateKeyPairPem();
	mkdirSync(path.dirname(privatePath), { recursive: true });
	mkdirSync(path.dirname(publicPath), { recursive: true });
	const publicBody = `# Public key for the Persia DB waitlist. The private key is not in this repository.\n${publicKey.endsWith('\n') ? publicKey : `${publicKey}\n`}`;
	writeFileSync(publicPath, publicBody);
	writeFileSync(privatePath, privateKey.endsWith('\n') ? privateKey : `${privateKey}\n`, {
		mode: 0o600,
	});
	chmodSync(privatePath, 0o600);
	console.log(`public key: ${publicPath}`);
	console.log(`private key: ${privatePath}`);
	console.log('The private key stays on this machine. Do not commit it.');
}

const invoked =
	process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (invoked) {
	try {
		main();
	} catch (error) {
		console.error(error instanceof Error ? error.message : 'keygen failed');
		process.exit(1);
	}
}
