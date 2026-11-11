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

test('the live tree matches the write path that is actually checked in', () => {
	const m5 = progress.milestones.find((ms) => ms.id === 'M5');
	const m6 = progress.milestones.find((ms) => ms.id === 'M6');
	const m7 = progress.milestones.find((ms) => ms.id === 'M7');
	assert.equal(m5?.state, 'done');
	assert.equal(m6?.state, 'in progress');
	assert.equal(m7?.state, 'in progress');
	assert.ok((m7?.done ?? 0) > 0);
	assert.equal(status.walReady, true);
	for (const text of [status.engineShort, status.engineNow, status.libraryNow]) {
		assert.match(text, /WAL with chunks, group commit, and replay/);
		assert.match(text, /query path is in progress/);
		assert.match(text, /BM25F scoring/);
		assert.match(text, /A collection snapshot can be searched as library code/);
		assert.equal(text.includes('Nothing searches'), false);
		assert.equal(text.includes('There is no query engine'), false);
		assert.equal(text.includes('No WAL'), false);
		assert.equal(text.includes('no memtable'), false);
		assert.equal(text.includes('M5 in progress'), false);
		assert.equal(text.includes('"'), false);
	}
	assert.match(status.preAlpha, /runs a ranked search over a collection snapshot/);
	assert.equal(status.preAlpha.includes('Nothing runs a search'), false);
	assert.match(status.preAlpha, /can put, delete, and commit/);
	assert.equal(status.preAlpha.includes('"'), false);
	assert.match(status.engineShort, /tombstone sidecars/);
	assert.match(status.engineShort, /segment merging/);
	assert.match(status.engineShort, /filter DSL/);
	assert.match(status.engineShort, /Relaxation is in progress \(M8, \d+\/\d+\): only the exact level builds a query/);
	assert.match(status.libraryNow, /content hashing, chunking, and blob manifests/);
	assert.equal(status.libraryNow.includes('exports no API'), false);
	assert.match(status.relaxationNow, /Levels after exact do not build a query/);
	assert.match(status.blobNow, /The pack file, remote blobs, document integration, and garbage collection are not written/);
	assert.match(status.queryPathNow, /filters, sorts, cursor pages, facets, deadlines, and explain/);
	assert.equal(status.engineShort.includes('Open and recovery orchestration is only partly done'), false);
	assert.match(status.engineNow, /Public modules: .*wal/);
	assert.equal(status.engineModules.split(', ').includes('query'), true);
	assert.equal(status.engineModules.split(', ').includes('score'), true);
	assert.equal(status.engineModules.split(', ').includes('stats'), true);
	assert.match(status.engineCalls, /Database has open, create_collection, put, put_batch, delete, and commit/);
	assert.match(status.engineCalls, /no persia::Db/);
	assert.match(status.engineCalls, /CollectionViews::search runs a ranked search over a collection snapshot/);
	assert.match(status.engineCalls, /no search on the database handle/);
	assert.match(status.writePathNow, /replays the WAL/);
	assert.match(status.writePathNow, /A collection snapshot can be searched as library code/);
	assert.match(status.writePathNow, /Open and recovery orchestration is done/);
	assert.match(status.noPublicApi, /exports no Db and no Collection/);
	assert.match(status.noPublicApi, /A search runs only through persia-engine/);
	assert.match(status.noPublicApi, /unpublished/);
	assert.match(status.m10Blocked, /M6 \(in progress\)/);
	assert.match(status.m10Blocked, /M7 \(in progress\)/);
	assert.match(status.m5Cell, /Done \(M5,/);
	assert.match(status.m5Cell, /Segments are searched through CollectionViews::search/);
	assert.match(status.pillarEngine, /WAL, commit, and segment merging/);
	assert.match(status.pillarEngine, /BM25F scoring and a collection search are in the tree/);
	assert.match(status.pillarEngine, /no search call on the database handle/);
	assert.match(status.tokens['%%FEATURES_LEAD%%'], /run as library code in persia-engine/);
	assert.match(status.tokens['%%FEATURES_LEAD%%'], /a search that matches nothing returns no hits/);
	assert.match(status.tokens['%%QUERY_CELL%%'], /return ranked hits through CollectionViews::search/);
	assert.match(
		status.tokens['%%MATCHING_CELL%%'],
		/filters, sorts, cursor pages, facets, deadlines, and explain, through CollectionViews::search/,
	);
	assert.match(status.tokens['%%RELAXATION_CELL%%'], /only the exact level builds a query, so a typo still returns no hits/);
	assert.match(status.tokens['%%RELAXATION_NOW%%'], /search_relaxed/);
	assert.match(status.tokens['%%BLOB_CELL%%'], /The pack file is not written/);
	assert.match(status.tokens['%%STATE_M8%%'], /^In progress \(M8, \d+\/\d+\)\.$/);
	assert.match(status.tokens['%%OPEN_CELL%%'], /Database opens a file and CollectionViews::search searches it/);
	assert.match(status.tokens['%%STATE_M9%%'], /^In progress \(M9, \d+\/\d+\)\.$/);
	assert.match(status.tokens['%%STATE_M1%%'], /^Done \(M1, \d+\/\d+\)\.$/);
	assert.equal(status.pillarEngine.includes('Scoring and query execution do not.'), false);
	assert.equal(status.pillarCorrectness.includes('no document write path'), false);
	assert.match(status.pillarCloud, /Object storage has not been started \(M12\)/);
	assert.equal(status.writePathLead, 'Local write path, partly in the tree');
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
	assert.match(faq, /A collection snapshot can be searched as library code/);
	assert.match(faq, /BM25F scoring/);
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
		hasDeadline: true,
		hasExplain: true,
		analyzers: ['de', 'en'],
		published: false,
		version: '0.0.0',
	};
}

test('an unstarted relaxation milestone stays unclaimed', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	for (const id of ['8.1.1', '8.1.2', '8.1.3', '8.1.4', '8.1.5', '8.2.1', '8.2.2.1', '8.2.3', '8.2.4.1', '8.2.5']) {
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
