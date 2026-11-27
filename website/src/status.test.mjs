import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { loadProgress, parseProgress } from './progress.mjs';
import {
	buildStatus,
	exportedFns,
	implHasMethod,
	loadStatus,
	parseRoadmapLeaves,
	publicModules,
	statusTokenNames,
	substituteStatusText,
} from './status.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const docsDir = path.join(here, 'content/docs');

const status = loadStatus();
const progress = loadProgress();

/**
 * @param {string} dir
 * @returns {string[]}
 */
function walk(dir) {
	/** @type {string[]} */
	const files = [];
	for (const name of readdirSync(dir)) {
		const full = path.join(dir, name);
		if (statSync(full).isDirectory()) files.push(...walk(full));
		else if (name.endsWith('.md') || name.endsWith('.mdx')) files.push(full);
	}
	return files;
}

test('the live tree names the embedded API, not the engine', () => {
	for (const id of ['M5', 'M6', 'M7', 'M8', 'M9']) {
		assert.equal(progress.milestones.find((ms) => ms.id === id)?.state, 'done', id);
	}
	assert.equal(status.walReady, true);
	for (const text of [status.engineShort, status.engineNow, status.libraryNow]) {
		assert.match(text, /WAL with chunks, group commit, and replay/);
		assert.match(text, /segment merging/);
		assert.equal(text.includes('Nothing searches'), false);
		assert.equal(text.includes('"'), false);
	}
	// ROADMAP 10.1.41: the facade's calls, from crates/persia/public-api.txt.
	assert.match(
		status.engineCalls,
		/^The embedded API is persia::Db \(open, create_collection, collection, put_blob, get_blob, commit, backup, and close\) and persia::Collection \(put, put_json, put_struct, get, delete, search, and search_json\)\./,
	);
	assert.match(status.libraryNow, /The persia crate is the embedded API: persia::Db and persia::Collection\./);
	assert.match(status.preAlpha, /Collection::search runs a ranked search/);
	assert.match(status.writePathNow, /Opening a database reloads the committed manifest/);
	assert.match(status.tokens['%%QUERY_CELL%%'], /return ranked hits through Collection::search/);
	assert.match(
		status.tokens['%%MATCHING_CELL%%'],
		/filters, sorts, cursor pages, facets, and explain, through Collection::search/,
	);
	// A SearchRequest has no deadline setter (ROADMAP 10.1.44): no token may offer one through the facade.
	const snapshot = readFileSync(path.join(root, 'crates/persia/public-api.txt'), 'utf8');
	if (!/^pub fn persia::SearchRequest::deadline[(<]/m.test(snapshot)) {
		for (const token of ['%%MATCHING_CELL%%', '%%QUERY_PATH_NOW%%', '%%QUERY_CELL%%', '%%FEATURES_LEAD%%']) {
			assert.equal(status.tokens[token].includes('deadline'), false, `${token} offers a deadline`);
		}
	}
	assert.match(status.tokens['%%FEATURES_LEAD%%'], /run through Collection::search/);
	assert.match(status.tokens['%%OPEN_CELL%%'], /Db::open and OpenOptions::open open a database file/);
	assert.match(status.m5Cell, /Segments are searched through Collection::search/);
	assert.match(status.tokens['%%RELAXATION_CELL%%'], /^Done \(M8, \d+\/\d+\)\./);
	assert.match(status.tokens['%%BLOB_CELL%%'], /^The blob store is done \(M9, \d+\/\d+\)\.$/);
	assert.match(status.tokens['%%STATE_M1%%'], /^Done \(M1, \d+\/\d+\)\.$/);
	assert.match(status.pillarEngine, /Scoring and query execution are done/);
	assert.match(status.pillarCloud, /Object storage has not been started \(M12\)/);
	for (const [name, text] of Object.entries(status.tokens)) {
		for (const engine of ENGINE_NAMES) {
			assert.equal(text.includes(engine), false, `${name} names ${engine}`);
		}
	}
});

/**
 * Engine items a guide must not present as API (ROADMAP 10.1.41; the same list as ENGINE_ONLY and ENGINE_CRATES in
 * xtask/src/repo_checks/guide_api.rs, which reads the pages but not the sentences these tokens expand to).
 */
const ENGINE_NAMES = [
	'persia_engine',
	'persia_blob',
	'persia::internal',
	'CollectionViews',
	'CollectionSnapshot',
	'RelaxPlan',
	'QueryCompiler',
	'QueryLimits',
	'SearchPlan',
	'Database::',
	'BlobDatabase',
];

test('the status sentences on the guide pages name no engine item', () => {
	const guides = path.join(docsDir, 'guide');
	for (const name of readdirSync(guides)) {
		const raw = readFileSync(path.join(guides, name), 'utf8');
		for (const token of statusTokenNames().filter((token) => raw.includes(token))) {
			const text = status.tokens[token];
			for (const engine of ENGINE_NAMES) {
				assert.equal(text.includes(engine), false, `guide/${name}: ${token} names ${engine}`);
			}
		}
	}
});

test('an unchecked pack leaf still says the pack file is not written', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	live.set('9.2.1', false);
	const input = currentInput(live);
	const progressCopy = structuredClone(input.progress);
	const m9 = progressCopy.milestones.find((ms) => ms.id === 'M9');
	if (!m9) throw new Error('missing M9');
	m9.state = 'in progress';
	input.progress = progressCopy;
	input.blobExports = true;
	const next = buildStatus(input);
	assert.match(
		next.blobNow,
		/The pack file, remote blobs, document integration, and garbage collection are not written/,
	);
	assert.match(next.tokens['%%BLOB_CELL%%'], /The pack file is not written/);
});

test('an unchecked WAL leaf drops the full WAL claim', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	live.set('6.2.2', false);
	const input = currentInput(live);
	const next = buildStatus(input);
	assert.equal(next.walReady, false);
	assert.match(next.engineShort, /a WAL \(not every leaf is done\)/);
	assert.equal(next.engineShort.includes('group commit, and replay'), false);
	assert.equal(next.writePathNow.includes('replays the WAL'), false);
});

test('the collection search is claimed only with its leaves checked and its code in the tree', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	const progressCopy = structuredClone(input.progress);
	const m7 = progressCopy.milestones.find((ms) => ms.id === 'M7');
	if (!m7) throw new Error('missing M7');
	m7.state = 'in progress';
	input.progress = progressCopy;
	assert.match(buildStatus(input).engineShort, /A collection snapshot can be searched as library code/);
	input.hasCollectionSearch = false;
	assert.throws(() => buildStatus(input), /marks request done but the matching code is not in the tree/);
	input.hasCollectionSearch = true;
	live.set('7.5.2', false);
	const partial = buildStatus(input);
	assert.match(partial.engineShort, /part of the collection search \(not every leaf is done\)/);
	assert.match(partial.engineShort, /A collection search call is in the tree, but not every leaf of it is done/);
	assert.equal(partial.engineShort.includes('Nothing searches'), false);
	assert.equal(partial.engineCalls.includes('CollectionViews::search'), false);
	assert.match(partial.tokens['%%QUERY_CELL%%'], /They do not return hits/);
});

test('a query compiler does not count as a search', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	for (const id of ['7.5.1', '7.5.2', '7.5.3', '7.5.4']) live.set(id, false);
	const input = currentInput(live);
	input.hasCollectionSearch = false;
	const progressCopy = structuredClone(input.progress);
	const m7 = progressCopy.milestones.find((ms) => ms.id === 'M7');
	if (!m7) throw new Error('missing M7');
	m7.state = 'in progress';
	input.progress = progressCopy;
	const next = buildStatus(input);
	assert.match(next.engineShort, /query path is in progress/);
	assert.match(next.engineShort, /BM25F scoring/);
	assert.match(next.engineShort, /Nothing searches/);
	assert.equal(next.engineShort.includes('There is no query engine'), false);
	input.hasSearch = true;
	const searching = buildStatus(input);
	assert.equal(searching.engineShort.includes('Nothing searches'), false);
	assert.match(searching.engineCalls, /no persia::Db/);
	assert.equal(searching.engineCalls.includes('no search'), false);
});

test('an unstarted query milestone says there is no query engine', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	for (const id of [
		'7.1.1',
		'7.1.2.1',
		'7.1.2.2',
		'7.1.3',
		'7.1.4',
		'7.2.1',
		'7.2.2',
		'7.3.1',
		'7.3.2',
		'7.3.3',
		'7.3.4',
		'7.4.1',
		'7.4.2',
		'7.4.3',
		'7.4.4.1',
		'7.4.4.2',
		'7.4.4.3',
		'7.5.1',
		'7.5.2',
		'7.5.3',
		'7.5.4',
	]) {
		live.set(id, false);
	}
	const input = currentInput(live);
	input.modules = input.modules.filter(
		(name) => name !== 'query' && name !== 'score' && name !== 'stats' && name !== 'search',
	);
	input.hasQuery = false;
	input.hasCollectionSearch = false;
	input.hasSearch = false;
	const progressCopy = structuredClone(input.progress);
	const m7 = progressCopy.milestones.find((ms) => ms.id === 'M7');
	if (!m7) throw new Error('missing M7');
	m7.state = 'blocked';
	m7.done = 0;
	input.progress = progressCopy;
	const next = buildStatus(input);
	assert.match(next.engineShort, /There is no query engine/);
	assert.match(next.engineShort, /Nothing searches/);
	assert.match(next.preAlpha, /Nothing runs a search/);
	assert.equal(next.preAlpha.includes('BM25F scorer'), false);
});

test('public modules follow lib.rs order', () => {
	const mods = publicModules(readFileSync(path.join(root, 'crates/persia-engine/src/lib.rs'), 'utf8'));
	assert.deepEqual(mods, [
		'db',
		'document',
		'manifest',
		'memtable',
		'merge',
		'query',
		'relax',
		'schema',
		'score',
		'search',
		'segment',
		'stats',
		'tombstone',
		'view',
		'wal',
	]);
	const fns = exportedFns(readFileSync(path.join(root, 'crates/persia-engine/src/db.rs'), 'utf8'));
	assert.equal(fns.has('put'), true);
	assert.equal(fns.has('search'), false);
});

test('docs pages do not keep the stale inventory, and tokens expand', () => {
	const banned = [
		'No WAL',
		'no WAL',
		'no memtable',
		'M5 in progress',
		'no document write path',
		'does not open a database, index a document',
		'Query, the WAL, and relaxation do not',
		'index a document, or answer a query',
		'There is still no query',
		'There is no query engine',
		'BM25F scoring and query execution are not implemented',
		'BM25F, filters, facets, and query execution are not implemented',
		'The query engine is not built',
		'The query engine that would call the trait is not written',
		'Scoring and query execution do not.',
		'They do not run inside a query',
		'They do not run a query.',
		'it is not in the tree yet',
		'Search, ranking, and relaxation are still **planned**',
		'Search, ranking, relaxation, blobs, and the server are **planned**.',
		'Nothing runs a query',
		'does not answer a query',
		'not a search you can call',
		'is an empty `main`',
		'is an empty main function',
		'Empty `main`',
		'do not return hits.',
		'does not search yet',
		'cannot answer a query',
		'There is no search call.',
		'It has no `search`',
		'They do not search.',
		'Not started (ROADMAP 7.4.5',
		'Not a search call',
		'A score is not a hit list',
		'They do not run a search',
		'Facets are not implemented',
		'The blob store has not been started',
		'persia-blob exports no API',
		'The pack file is not written',
		'planned content-addressed pack',
		'persia-blob and persia-proto are module docs only',
		'The persia crate exports a public API.',
		'The cascade does not run',
		'The cascade has not been started',
		'Evaluating a filter against documents is ROADMAP 7.4.5 and has not been started',
		'Searching segments, merging hits, and the cascade are not',
		'Searching segments and the relaxation cascade are not',
		'No API yet (M9)',
		'Nothing in the repository computes this',
		'returned by nothing today',
		'it has no cancel flag',
		'Query execution that would search through it is not written',
		'Not started (M8)',
		'Module docs only.',
		'The relaxation milestone below has not been started',
		'Nothing yet evaluates a filter',
		'No scan runs',
		'No automaton runs',
		'Nothing matches it against postings',
		'A search that returns hits is still not there',
		'Not started: filter bitmaps',
		'and the cascade do not',
		'no code lists the terms within that distance',
		'rejects `prefix`, `fuzzy`',
	];
	for (const file of walk(docsDir)) {
		const raw = readFileSync(file, 'utf8');
		const filled = substituteStatusText(raw);
		for (const phrase of banned) {
			assert.equal(raw.includes(phrase), false, `${path.relative(docsDir, file)} still says ${JSON.stringify(phrase)}`);
			assert.equal(filled.includes(phrase), false, `${path.relative(docsDir, file)} expands to ${JSON.stringify(phrase)}`);
		}
		for (const token of statusTokenNames()) {
			assert.equal(filled.includes(token), false, `${path.relative(docsDir, file)} left ${token}`);
		}
	}
	const faq = substituteStatusText(readFileSync(path.join(docsDir, 'guide/faq.md'), 'utf8'));
	assert.match(faq, /WAL with chunks, group commit, and replay/);
	// The cascade is done (M8): the FAQ states the finished milestone, not the in-progress "every cascade level builds a
	// query" sentence of before.
	assert.match(faq, /Done \(M8, \d+\/\d+\)\. L4 fuzzy reports a correction/);
	assert.match(faq, /The embedded API is persia::Db/);
	assert.match(faq, /BM25F scorer/);
	assert.equal(faq.includes('There is no query engine'), false);
	const code = substituteStatusText(readFileSync(path.join(docsDir, 'guide/code.md'), 'utf8'));
	assert.match(code, /Public modules:/);
	assert.equal(code.includes('No WAL and no query'), false);
});

/**
 * @param {Map<string, boolean>} leaves
 */
function currentInput(leaves) {
	return {
		progress: loadProgress(),
		leaves,
		modules: [
			'db',
			'document',
			'manifest',
			'memtable',
			'merge',
			'query',
			'schema',
			'score',
			'search',
			'segment',
			'stats',
			'tombstone',
			'view',
			'wal',
		],
		dbFns: new Set(['open', 'create_collection', 'put', 'put_batch', 'delete', 'commit']),
		snapshotFns: new Set(['get']),
		persiaExports: false,
		blobExports: false,
		hasOpenOptions: false,
		hasPutStruct: false,
		hasPutBlob: false,
		hasGetBlob: false,
		hasQuery: true,
		hasSearch: false,
		hasCollectionSearch: true,
		hasRelaxSearch: true,
		hasDropTerms: true,
		hasSynonyms: true,
		hasPrefix: true,
		hasFuzzy: true,
		hasFallback: true,
		hasFilters: true,
		hasTiers: true,
		hasRelaxOptions: true,
		hasExplainLevel: true,
		hasDeadline: true,
		hasExplain: true,
		analyzers: ['de', 'en'],
		published: false,
		version: '0.0.0',
	};
}

test('an unstarted relaxation milestone stays unclaimed', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	for (const id of ['8.1.1', '8.1.2', '8.1.3', '8.1.4', '8.1.5', '8.2.1', '8.2.2.1', '8.2.2.2', '8.2.2.3', '8.2.3', '8.2.4.1', '8.2.5']) {
		live.set(id, false);
	}
	const input = currentInput(live);
	input.hasRelaxSearch = false;
	const progressCopy = structuredClone(input.progress);
	const m8 = progressCopy.milestones.find((ms) => ms.id === 'M8');
	if (!m8) throw new Error('missing M8');
	m8.state = 'blocked';
	m8.done = 0;
	input.progress = progressCopy;
	const next = buildStatus(input);
	assert.match(next.tokens['%%RELAXATION_CELL%%'], /^Not started \(M8\)\. On by default in the design\.$/);
	assert.match(next.engineShort, /Relaxation has not been started \(M8\)/);
});

test('drop_terms marked done without its transform is rejected', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	input.hasDropTerms = false;
	assert.throws(() => buildStatus(input), /marks drop_terms done but the matching code is not in the tree/);
});

test('unchecked synonym and prefix leaves stay unclaimed', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	live.set('8.2.1', false);
	live.set('8.2.3', false);
	const next = buildStatus(currentInput(live));
	assert.match(next.tokens['%%RELAXATION_CELL%%'], /L0 exact and L2 drop_terms build queries/);
	assert.match(next.tokens['%%RELAXATION_CELL%%'], /Synonyms and prefix are not built/);
	assert.equal(next.relaxationNow.includes('A one-term typo still returns no hits'), false);
	assert.equal(next.relaxationNow.includes('A typo is not corrected'), false);
	live.set('8.2.4.1', false);
	live.set('8.2.5', false);
	const none = buildStatus(currentInput(live));
	assert.match(none.tokens['%%RELAXATION_CELL%%'], /Synonyms, prefix, fuzzy, and the fallback are not built/);
	assert.match(none.relaxationNow, /A typo is not corrected/);
	assert.match(none.relaxationNow, /A one-term typo still returns no hits/);
});

test('synonyms marked done without its transform is rejected', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	input.hasSynonyms = false;
	assert.throws(() => buildStatus(input), /marks synonyms done but the matching code is not in the tree/);
});

test('prefix marked done without its transform is rejected', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	input.hasPrefix = false;
	assert.throws(() => buildStatus(input), /marks prefix done but the matching code is not in the tree/);
});

test('fuzzy, fallback, and filter relaxation marked done without their transforms are rejected', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const fuzzy = currentInput(live);
	fuzzy.hasFuzzy = false;
	assert.throws(() => buildStatus(fuzzy), /marks fuzzy done but the matching code is not in the tree/);
	const fallback = currentInput(live);
	fallback.hasFallback = false;
	assert.throws(() => buildStatus(fallback), /marks fallback done but the matching code is not in the tree/);
	const filters = currentInput(live);
	filters.hasFilters = false;
	assert.throws(() => buildStatus(filters), /marks filter relaxation done but the matching code is not in the tree/);
	const tiers = currentInput(live);
	tiers.hasTiers = false;
	assert.throws(() => buildStatus(tiers), /marks typo tiers done but the matching code is not in the tree/);
});

test('relaxation marked done without search_relaxed is rejected', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	input.hasRelaxSearch = false;
	assert.throws(() => buildStatus(input), /marks relaxation done but the matching code is not in the tree/);
});

test('implHasMethod looks inside the impl block only', () => {
	const plain = "impl CollectionViews<'_> {\n    /// { braces in docs }\n    pub fn search(&self) {}\n}\n";
	assert.equal(implHasMethod(plain, 'CollectionViews', 'search'), true);
	const generic = "impl<'a> CollectionViews<'a> {\n    pub fn search<V>(&self) {}\n}\n";
	assert.equal(implHasMethod(generic, 'CollectionViews', 'search'), true);
	const elsewhere = "impl CollectionViews<'_> {\n    pub fn other(&self) { if x { y } }\n}\n\nimpl Other {\n    pub fn search(&self) {}\n}\n";
	assert.equal(implHasMethod(elsewhere, 'CollectionViews', 'search'), false);
	assert.equal(implHasMethod("impl CollectionViews<'_> {\n    fn search(&self) {}\n}\n", 'CollectionViews', 'search'), false);
	assert.equal(implHasMethod("impl CollectionViewsX {\n    pub fn search(&self) {}\n}\n", 'CollectionViews', 'search'), false);
});
