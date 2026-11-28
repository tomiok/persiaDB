// website.yml's paths filter covers the repository files the site reads outside website/ (ROADMAP 10.1.52).
// status.mjs reads engine and facade sources, the public-API snapshot, Cargo.toml, ROADMAP.md and PROGRESS.md at
// build time, and the node tests read some of them too. A change to one of them changes the pages, so the website
// workflow must run on it. This test scans the website's code (every .mjs, .js, .ts, .astro and .mdx file under
// website/, outside node_modules and build output) for the paths it names, and fails unless each one matches a
// pattern of the `pull_request` paths list, and unless the `push` list is the same. The scan is static: it reads the
// path forms `repoReads` lists and fails on a file-system call whose path is in none of them, but it does not follow
// variables, so a repository path that reaches one of the bases it trusts (`REPO_BASES`, `WEBSITE_BASES`,
// `DELEGATED`) through a variable is not seen. Node built-ins only: it runs in the website workflow's `Status and
// layer tests` step, before `npm ci`.
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const workflow = '.github/workflows/website.yml';
const self = 'website/src/paths-filter.test.mjs';

/**
 * Whether `file` (a repository path) matches the paths-filter `pattern`, as GitHub reads these patterns: `**`
 * matches any characters, `/` included, and also nothing when it is a whole segment (so `a/` `**` `/b` matches `a/b`,
 * and a leading `**` `/` matches a top-level file); `*` matches any characters but `/`. The filters here need nothing
 * else, so a pattern with another special character (`?`, `+`, `[`, `{`, a leading `!`) is refused rather than
 * misread.
 * @param {string} pattern
 * @param {string} file
 */
export function globMatches(pattern, file) {
	if (/[?+[\]{}]|^!/.test(pattern)) throw new Error(`${pattern}: only * and ** are supported`);
	const piece = (text) =>
		text
			.split('**')
			.map((part) =>
				part
					.split('*')
					.map((chars) => chars.replace(/[.^$|()\\]/g, '\\$&'))
					.join('[^/]*'),
			)
			.join('.*');
	const leading = pattern.startsWith('**/');
	const body = (leading ? pattern.slice(3) : pattern).split('/**/').map(piece).join('(?:/|/.*/)');
	return new RegExp(`^${leading ? '(?:.*/)?' : ''}${body}$`).test(file);
}

/**
 * The `paths` list of the workflow's `on.<event>` trigger, read line by line: the `paths:` line under
 * `  <event>:`, then its `      - '...'` items (an item may end in a `# comment`; comment lines and blank lines are
 * skipped) up to the first line indented less. A line indented 6 or more that is not such an item is an error, so a
 * list this reader cannot read fails instead of being cut short.
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
		const item = /^ {6}- (?:'([^']*)'|"([^"]*)"|([^\s#'"]+))(?:\s+#.*)?\s*$/.exec(line);
		if (!item) {
			assert.ok(!/^ {6}/.test(line), `${workflow}: cannot read the \`${event}\` paths item ${JSON.stringify(line)}`);
			break;
		}
		paths.push(item[1] ?? item[2] ?? item[3]);
	}
	assert.ok(paths.length > 0, `${workflow}: the \`${event}\` paths list is empty`);
	return paths;
}

/**
 * The arguments of the call whose `(` is at `open` in `source`, split at its top-level commas and trimmed, or
 * `undefined` when the call does not close.
 * @param {string} source
 * @param {number} open
 * @returns {string[] | undefined}
 */
function callArgs(source, open) {
	const args = [];
	let depth = 0;
	let from = open + 1;
	for (let at = open + 1; at < source.length; at += 1) {
		const c = source[at];
		if (c === "'" || c === '"' || c === '`') {
			at += 1;
			while (at < source.length && source[at] !== c) at += source[at] === '\\' ? 2 : 1;
		} else if ('([{'.includes(c)) depth += 1;
		else if (')]}'.includes(c) && depth > 0) depth -= 1;
		else if (c === ')' || (c === ',' && depth === 0)) {
			args.push(source.slice(from, at).trim());
			if (c === ')') return args.filter((arg, i) => arg.length > 0 || i < args.length - 1);
			from = at + 1;
		}
	}
	return undefined;
}

/**
 * The text of a string literal with no escape and no substitution, else `undefined`.
 * @param {string} text
 */
function literal(text) {
	return /^(['"`])([^'"`$\\]*)\1$/.exec(text)?.[2];
}

/** Bases that are the repository root, or a repository file whose literal the scan reads where it is defined. */
const REPO_BASES = new Set(['root', 'fromHere', 'fromCwd']);

/** The directory of the scanned file itself: a `../` literal joined on it is resolved like any other. */
const FILE_DIR_BASES = new Set(['here', 'path.dirname(fileURLToPath(import.meta.url))']);

/**
 * Bases inside website/, by file: the docs directory, the build output and the walk and loop variables over them.
 * Kept per file because names such as `file` and `dir` say nothing about where they point.
 * @type {Record<string, string[]>}
 */
const WEBSITE_BASES = {
	'website/src/llms-index.mjs': ['dist', 'dir', 'full', 'file'],
	'website/src/status.test.mjs': ['docsDir', 'guides', 'dir', 'full', 'file'],
};

/**
 * Whole file-system call paths checked where they come from: status.mjs's `read` helper (each `read('<path>')` call
 * is scanned) and progress.mjs's `progressFile()` (which returns its `../../PROGRESS.md` and `../PROGRESS.md`
 * literals).
 * @type {Record<string, string[]>}
 */
const DELEGATED = {
	'website/src/status.mjs': ['path.join(root, rel)'],
	'website/src/progress.mjs': ['progressFile()'],
};

/**
 * The file whose `read('<path>')` calls are its repository reader. Elsewhere `read` is another function (a stream's
 * `async read()`), and a reader written there is caught by its own file-system call, which is not in `DELEGATED`.
 */
const READER = 'website/src/status.mjs';

const FS_READ =
	/(?<![\w$])(?:readFileSync|readFile|existsSync|readdirSync|opendirSync|statSync|lstatSync|accessSync|openSync|createReadStream)\(/g;

/**
 * The repository paths `source` (the repository file `file`) reads, from the forms the website's code uses:
 * - in status.mjs (`READER`), `read('<path>')`, its reader, relative to the repository root;
 * - `path.join(root | fromHere | fromCwd, '<path>')` (or `path.resolve`), a join on the repository root;
 * - `path.resolve(process.cwd(), '../<path>')`, relative to website/, where `npm run build` runs;
 * - any other `'../<path>'` literal (an import, a `new URL`, `path.resolve(here, '../../<path>')`), relative to the
 *   directory of `file`; a `?query` is dropped, and one that stays inside website/ or names the root is not a read.
 * `opaque` lists what the scan cannot hold to the filter: a `read(...)` without a literal path, a `../` literal that
 * leaves the repository, and a file-system call (`readFileSync`, `existsSync`, `readdirSync`, ...) whose path is not
 * a known base (`REPO_BASES` joined on literals, the file's own directory joined on names without `..` or on one
 * `../` literal, `WEBSITE_BASES` joined on names without `..`) or a path checked where it comes from (`DELEGATED`).
 * @param {string} source
 * @param {string} file
 */
export function repoReads(source, file) {
	const found = [];
	const opaque = [];
	const dir = path.posix.dirname(file);
	const repoPath = (rel, what) => {
		const resolved = path.posix.normalize(rel.replace(/[?#].*$/, ''));
		if (resolved === '.' || resolved === 'website' || resolved.startsWith('website/')) return;
		if (resolved === '..' || resolved.startsWith('../')) opaque.push(what);
		else found.push(resolved);
	};
	for (const m of file === READER ? source.matchAll(/(?<![\w.$])read\(\s*([^)]*)\)/g) : []) {
		const lit = literal(m[1].trim());
		if (lit !== undefined) found.push(lit);
		else opaque.push(m[0]);
	}
	for (const m of source.matchAll(/path\.(?:join|resolve)\(\s*(?:root|fromHere|fromCwd)\s*,\s*(['"`])([^'"`$]+)\1/g)) {
		found.push(m[2]);
	}
	for (const m of source.matchAll(/process\.cwd\(\)\s*,\s*(['"`])(\.\.(?:\/[^'"`$]*)?)\1/g)) {
		repoPath(path.posix.join('website', m[2]), m[0]);
	}
	for (const m of source.matchAll(/(?<!process\.cwd\(\)\s*,\s*)(['"`])(\.\.(?:\/[^'"`$\s]*)?)\1/g)) {
		repoPath(path.posix.join(dir, m[2]), m[0]);
	}
	const websiteBases = new Set(WEBSITE_BASES[file] ?? []);
	const delegated = new Set(DELEGATED[file] ?? []);
	for (const m of source.matchAll(FS_READ)) {
		const arg = callArgs(source, m.index + m[0].length - 1)?.[0];
		if (arg === undefined) {
			opaque.push(`${m[0]})`);
			continue;
		}
		if (delegated.has(arg)) continue;
		const join = /^path\.(?:join|resolve)\(/.exec(arg);
		const [base, ...rest] = (join ? callArgs(arg, join[0].length - 1) : [arg]) ?? [];
		const lits = rest.map(literal);
		const climbs = lits.some((lit) => lit !== undefined && lit.split('/').includes('..'));
		let known = false;
		if (REPO_BASES.has(base)) known = lits.every((lit) => lit !== undefined);
		else if (FILE_DIR_BASES.has(base)) known = !climbs || (rest.length === 1 && lits[0] !== undefined);
		else known = websiteBases.has(base) && !climbs;
		if (!known) opaque.push(`${m[0]}${arg})`);
	}
	return { found, opaque };
}

/**
 * The website's own code, as repository paths: every .mjs, .js, .ts, .astro and .mdx file under website/, outside
 * node_modules and build output. This file is left out: its own test strings name paths on purpose.
 * @param {string} dir
 * @returns {string[]}
 */
function sources(dir = 'website') {
	const files = [];
	for (const entry of readdirSync(path.join(root, dir), { withFileTypes: true })) {
		const rel = `${dir}/${entry.name}`;
		if (entry.isDirectory()) {
			if (!['node_modules', 'dist', '.astro'].includes(entry.name)) files.push(...sources(rel));
		} else if (/\.(mjs|js|ts|astro|mdx)$/.test(entry.name) && rel !== self) {
			files.push(rel);
		}
	}
	return files.sort();
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

test('the glob matcher reads a whole-segment ** as any number of segments, none included', () => {
	assert.ok(globMatches('a/**/b', 'a/b'));
	assert.ok(globMatches('a/**/b', 'a/x/b'));
	assert.ok(globMatches('a/**/b', 'a/x/y/b'));
	assert.ok(!globMatches('a/**/b', 'ab'));
	assert.ok(!globMatches('a/**/b', 'a/xb'));
	assert.ok(globMatches('**/lib.rs', 'lib.rs'));
	assert.ok(globMatches('**/lib.rs', 'crates/persia/src/lib.rs'));
	assert.ok(!globMatches('**/lib.rs', 'crates/persia/src/xlib.rs'));
});

test('the paths reader keeps an item with a trailing comment and refuses one it cannot read', () => {
	const on = (items) => ['on:', '  push:', '    paths:', ...items, '  workflow_dispatch:', ''].join('\n');
	assert.deepEqual(
		triggerPaths(on(["      - 'Cargo.toml' # root manifest", '      # a comment line', '', "      - 'ROADMAP.md'"]), 'push'),
		['Cargo.toml', 'ROADMAP.md'],
	);
	assert.deepEqual(triggerPaths(on(['      - PROGRESS.md   # unquoted']), 'push'), ['PROGRESS.md']);
	assert.throws(() => triggerPaths(on(["      - 'Cargo.toml'", "      - 'a' 'b'", "      - 'ROADMAP.md'"]), 'push'), /cannot read/);
	assert.throws(() => triggerPaths(on(["      - 'Cargo.toml'", "        - 'ROADMAP.md'"]), 'push'), /cannot read/);
});

test('the reader scan finds every form and refuses a path it cannot read', () => {
	const sample = [
		"const a = read('ROADMAP.md');",
		"readFileSync(path.join(root, 'crates/persia/public-api.txt'), 'utf8');",
		"path.resolve(here, '../../PROGRESS.md');",
		"path.resolve(process.cwd(), '../Cargo.toml');",
		"import x from '../../scripts/waitlist/keygen.mjs'; new URL('../../SPEC.md?raw', import.meta.url);",
		"import y from './layers.mjs'; import z from '../relay/x.mjs'; path.resolve(here, '../..');",
		'reader.read(buf); thread(x); const read = (rel) => text(rel);',
		"read(rel); read(`crates/${name}`); path.resolve(here, '../../../x');",
	].join('\n');
	const { found, opaque } = repoReads(sample, READER);
	assert.deepEqual(found.sort(), [
		'Cargo.toml',
		'PROGRESS.md',
		'ROADMAP.md',
		'SPEC.md',
		'crates/persia/public-api.txt',
		'scripts/waitlist/keygen.mjs',
	]);
	assert.deepEqual(opaque, ['read(rel)', 'read(`crates/${name}`)', "'../../../x'"]);
	// Outside status.mjs `read` is not the reader, and a reader there is refused by its own file-system call.
	assert.deepEqual(repoReads("async read() {} read('ROADMAP.md');", 'website/src/other.mjs'), { found: [], opaque: [] });
	assert.deepEqual(repoReads('const read = (rel) => readFileSync(path.join(root, rel));', 'website/src/other.mjs').opaque, [
		'readFileSync(path.join(root, rel))',
	]);
	// A `../` literal resolves against the scanned file's own directory.
	assert.deepEqual(repoReads("import k from '../../../scripts/a.mjs';", 'website/src/waitlist/t.mjs').found, ['scripts/a.mjs']);
	assert.deepEqual(repoReads("import k from '../../scripts/a.mjs';", 'website/src/waitlist/t.mjs').found, []);
});

test('the reader scan refuses a file-system call on a path it cannot resolve', () => {
	const file = 'website/src/status.test.mjs';
	const accepted = [
		"readFileSync(path.join(root, 'ROADMAP.md'), 'utf8');",
		'existsSync(fromHere);',
		"readFileSync(path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../PROGRESS.md'), 'utf8');",
		"readFileSync(path.join(here, 'public.pem'));",
		"readdirSync(guides); readFileSync(path.join(guides, name), 'utf8'); statSync(full); readFileSync(file);",
	].join('\n');
	assert.deepEqual(repoReads(accepted, file).opaque, []);
	for (const call of [
		"readFileSync(path.resolve(here, '..', '..', 'SPEC.md'))",
		"readFileSync(path.join(repo, 'SPEC.md'))",
		'readFileSync(spec)',
		"readFileSync('SPEC.md')",
		'fs.readFileSync(path.join(root, rel))',
		"readFileSync(path.join(docsDir, '..', 'x'))",
		'existsSync(progressFile())',
		'readFileSync()',
	]) {
		assert.deepEqual(repoReads(`${call};`, file).opaque, [call.replace(/^fs\./, '')], call);
	}
	assert.deepEqual(repoReads('const x = readFileSync(file);', 'website/src/other.mjs').opaque, ['readFileSync(file)']);
	assert.deepEqual(repoReads('readFileSync(progressFile());', 'website/src/progress.mjs').opaque, []);
});

test("website.yml's push and pull_request paths lists are the same", () => {
	assert.deepEqual(triggerPaths(yaml, 'push'), triggerPaths(yaml, 'pull_request'));
});

test("every repository file the website reads is in website.yml's pull_request paths filter", () => {
	const patterns = triggerPaths(yaml, 'pull_request');
	const scanned = sources();
	// The scan still walks into the directories it was widened to.
	for (const known of ['website/src/components/HeroVideo.astro', 'website/src/waitlist/roundtrip.test.mjs', 'website/astro.config.mjs']) {
		assert.ok(scanned.includes(known), `the scan no longer reads ${known}`);
	}
	const reads = new Map();
	const opaque = [];
	for (const file of scanned) {
		const scan = repoReads(readFileSync(path.join(root, file), 'utf8'), file);
		for (const rel of scan.found) reads.set(rel, [...(reads.get(rel) ?? []), file]);
		opaque.push(...scan.opaque.map((call) => `${file}: ${call}`));
	}
	assert.deepEqual(opaque, [], 'a read whose path the scan cannot resolve cannot be held to the filter');
	// The scan still sees the readers it was written for: status.mjs's sources, its snapshot, PROGRESS.md and the
	// waitlist test's key generator.
	for (const known of [
		'ROADMAP.md',
		'PROGRESS.md',
		'Cargo.toml',
		'crates/persia/public-api.txt',
		'crates/persia-engine/src/db/views.rs',
		'scripts/waitlist/keygen.mjs',
	]) {
		assert.ok(reads.has(known), `the scan no longer finds ${known}`);
	}
	const uncovered = [...reads]
		.filter(([rel]) => !patterns.some((pattern) => globMatches(pattern, rel)))
		.map(([rel, files]) => `${rel} (read by ${[...new Set(files)].join(', ')})`);
	assert.deepEqual(uncovered, [], `not covered by ${workflow}'s paths filter`);
});
