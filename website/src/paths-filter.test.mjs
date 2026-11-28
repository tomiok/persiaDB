// website.yml's paths filter covers every repository file the site reads outside website/ (ROADMAP 10.1.52).
// status.mjs reads engine and facade sources, the public-API snapshot, Cargo.toml, ROADMAP.md and PROGRESS.md at
// build time, and the node tests read some of them too. A change to one of them changes the pages, so the website
// workflow must run on it. This test collects the paths those files name and fails unless each one matches a
// pattern of the `pull_request` paths list, and unless the `push` list is the same. Node built-ins only: it runs in
// the website workflow's `Status and layer tests` step, before `npm ci`.
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const workflow = '.github/workflows/website.yml';

/**
 * Whether `file` (a repository path) matches the paths-filter `pattern`, as GitHub reads it: `**` matches any
 * characters, `/` included, and `*` any characters but `/`. The filters here need nothing else, so a pattern with
 * another special character (`?`, `+`, `[`, `{`, a leading `!`) is refused rather than misread.
 * @param {string} pattern
 * @param {string} file
 */
export function globMatches(pattern, file) {
	if (/[?+[\]{}]|^!/.test(pattern)) throw new Error(`${pattern}: only * and ** are supported`);
	const source = pattern
		.split('**')
		.map((part) =>
			part
				.split('*')
				.map((text) => text.replace(/[.^$|()\\]/g, '\\$&'))
				.join('[^/]*'),
		)
		.join('.*');
	return new RegExp(`^${source}$`).test(file);
}

/**
 * The `paths` list of the workflow's `on.<event>` trigger, read line by line: the `paths:` line under
 * `  <event>:`, then its `      - '...'` items (comments and blank lines skipped) up to the first other line.
 * @param {string} yaml
 * @param {string} event
 * @returns {string[]}
 */
export function triggerPaths(yaml, event) {
	const lines = yaml.split('\n');
	const start = lines.findIndex((line) => line.trimEnd() === `  ${event}:`);
	assert.ok(start >= 0, `${workflow}: no \`${event}:\` trigger`);
	let at = start + 1;
	while (at < lines.length && !/^ {4}paths:\s*$/.test(lines[at])) {
		assert.ok(/^ {4}|^\s*$/.test(lines[at]), `${workflow}: the \`${event}\` trigger has no \`paths:\` list`);
		at += 1;
	}
	assert.ok(at < lines.length, `${workflow}: the \`${event}\` trigger has no \`paths:\` list`);
	const paths = [];
	for (at += 1; at < lines.length; at += 1) {
		const line = lines[at];
		if (/^\s*(#.*)?$/.test(line)) continue;
		const item = /^ {6}- (?:'([^']*)'|"([^"]*)"|([^\s#'"]+))\s*$/.exec(line);
		if (!item) break;
		paths.push(item[1] ?? item[2] ?? item[3]);
	}
	assert.ok(paths.length > 0, `${workflow}: the \`${event}\` paths list is empty`);
	return paths;
}

/**
 * The repository paths `source` reads, from the forms the website's code uses for a file outside website/:
 * - `read('<path>')`, status.mjs's reader, relative to the repository root;
 * - `path.join(root | fromHere | fromCwd, '<path>')`, a join on the repository root;
 * - `'../../<path>'`, relative to website/src (`path.resolve(<this directory>, ...)`);
 * - `path.resolve(process.cwd(), '../<path>')`, relative to website/, where `npm run build` runs.
 * A `read(...)` call whose argument is not a string literal is reported in `opaque`: its path cannot be checked.
 * @param {string} source
 */
export function repoReads(source) {
	const found = [];
	const opaque = [];
	for (const m of source.matchAll(/(?<![\w.$])read\(\s*([^)]*)\)/g)) {
		const literal = /^(['"`])([^'"`$]*)\1$/.exec(m[1].trim());
		if (literal) found.push(literal[2]);
		else opaque.push(m[0]);
	}
	for (const m of source.matchAll(/path\.join\(\s*(?:root|fromHere|fromCwd)\s*,\s*(['"`])([^'"`$]+)\1/g)) {
		found.push(m[2]);
	}
	for (const m of source.matchAll(/(['"`])\.\.\/\.\.\/([^'"`$]+)\1/g)) found.push(m[2]);
	for (const m of source.matchAll(/process\.cwd\(\)\s*,\s*(['"`])\.\.\/([^'"`$]+)\1/g)) found.push(m[2]);
	return { found, opaque };
}

/** The website's own code that can read outside website/: website/src/*.{mjs,ts} and astro.config.mjs. */
function sources() {
	const files = readdirSync(here)
		// This file is left out: its own test strings name paths on purpose.
		.filter((name) => /\.(mjs|ts)$/.test(name) && name !== path.basename(fileURLToPath(import.meta.url)))
		.map((name) => path.join('website/src', name));
	return [...files, 'website/astro.config.mjs'].sort();
}

const yaml = readFileSync(path.join(root, workflow), 'utf8');

test('the glob matcher reads * within a segment and ** across them', () => {
	assert.ok(globMatches('crates/persia-engine/src/**', 'crates/persia-engine/src/db/views.rs'));
	assert.ok(globMatches('crates/*/public-api.txt', 'crates/persia/public-api.txt'));
	assert.ok(!globMatches('crates/*/lib.rs', 'crates/persia/src/lib.rs'));
	assert.ok(!globMatches('Cargo.toml', 'crates/persia/Cargo.toml'));
	assert.ok(!globMatches('PROGRESS.md', 'PROGRESS.mdx'));
	assert.ok(!globMatches('crates/persia/src/lib.rs', 'crates/persia/src/libXrs'));
	assert.throws(() => globMatches('crates/{a,b}/**', 'crates/a/x'), /only \* and \*\*/);
});

test('the reader scan finds every form and refuses a path it cannot read', () => {
	const sample = [
		"const a = read('ROADMAP.md');",
		"readFileSync(path.join(root, 'crates/persia/public-api.txt'), 'utf8');",
		"path.resolve(here, '../../PROGRESS.md');",
		"path.resolve(process.cwd(), '../Cargo.toml');",
		'reader.read(buf); thread(x); const read = (rel) => readFileSync(path.join(root, rel));',
		"read(rel); read(`crates/${name}`);",
	].join('\n');
	const { found, opaque } = repoReads(sample);
	assert.deepEqual(found, ['ROADMAP.md', 'crates/persia/public-api.txt', 'PROGRESS.md', 'Cargo.toml']);
	assert.deepEqual(opaque, ['read(rel)', 'read(`crates/${name}`)']);
});

test("website.yml's push and pull_request paths lists are the same", () => {
	assert.deepEqual(triggerPaths(yaml, 'push'), triggerPaths(yaml, 'pull_request'));
});

test("every repository file the website reads is in website.yml's pull_request paths filter", () => {
	const patterns = triggerPaths(yaml, 'pull_request');
	const reads = new Map();
	const opaque = [];
	for (const file of sources()) {
		const scan = repoReads(readFileSync(path.join(root, file), 'utf8'));
		for (const rel of scan.found) reads.set(rel, [...(reads.get(rel) ?? []), file]);
		opaque.push(...scan.opaque.map((call) => `${file}: ${call}`));
	}
	assert.deepEqual(opaque, [], 'a read() whose path is not a string literal cannot be held to the filter');
	// The scan still sees the readers it was written for: status.mjs's sources, its snapshot and PROGRESS.md.
	for (const known of ['ROADMAP.md', 'PROGRESS.md', 'Cargo.toml', 'crates/persia/public-api.txt', 'crates/persia-engine/src/db/views.rs']) {
		assert.ok(reads.has(known), `the scan no longer finds ${known}`);
	}
	const uncovered = [...reads]
		.filter(([rel]) => !patterns.some((pattern) => globMatches(pattern, rel)))
		.map(([rel, files]) => `${rel} (read by ${[...new Set(files)].join(', ')})`);
	assert.deepEqual(uncovered, [], `not covered by ${workflow}'s paths filter`);
});
