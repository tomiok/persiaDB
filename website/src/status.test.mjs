import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { loadProgress, parseProgress } from './progress.mjs';
import {
	buildStatus,
	exportedFns,
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
		assert.match(text, /Nothing searches/);
		assert.equal(text.includes('There is no query engine'), false);
		assert.equal(text.includes('No WAL'), false);
		assert.equal(text.includes('no memtable'), false);
		assert.equal(text.includes('M5 in progress'), false);
		assert.equal(text.includes('"'), false);
	}
	assert.match(status.preAlpha, /A query compiler and a BM25F scorer are in the tree/);
	assert.match(status.preAlpha, /Nothing runs a search/);
	assert.match(status.preAlpha, /can put, delete, and commit/);
	assert.equal(status.preAlpha.includes('"'), false);
	assert.match(status.engineShort, /tombstone sidecars/);
	assert.match(status.engineShort, /segment merging/);
	assert.match(status.engineShort, /filter DSL/);
	assert.match(status.engineShort, /Relaxation has not been started \(M8\)/);
	assert.equal(status.engineShort.includes('Open and recovery orchestration is only partly done'), false);
	assert.match(status.engineNow, /Public modules: .*wal/);
	assert.equal(status.engineModules.split(', ').includes('query'), true);
	assert.equal(status.engineModules.split(', ').includes('score'), true);
	assert.equal(status.engineModules.split(', ').includes('stats'), true);
	assert.match(status.engineCalls, /Database has open, create_collection, put, put_batch, delete, and commit/);
	assert.match(status.engineCalls, /no persia::Db/);
	assert.match(status.engineCalls, /no search/);
	assert.match(status.writePathNow, /replays the WAL/);
	assert.match(status.writePathNow, /Nothing searches/);
	assert.match(status.writePathNow, /Open and recovery orchestration is done/);
	assert.match(status.noPublicApi, /exports no Db and no Collection/);
	assert.match(status.noPublicApi, /Nothing runs a search/);
	assert.match(status.noPublicApi, /unpublished/);
	assert.match(status.m10Blocked, /M6 \(in progress\)/);
	assert.match(status.m10Blocked, /M7 \(in progress\)/);
	assert.match(status.m5Cell, /Done \(M5,/);
	assert.match(status.m5Cell, /A segment view can be matched/);
	assert.match(status.m5Cell, /There is no search call/);
	assert.match(status.pillarEngine, /WAL, commit, and segment merging/);
	assert.match(status.pillarEngine, /BM25F scoring is in the tree/);
	assert.match(status.pillarEngine, /Score top-k over one view returns hits/);
	assert.match(status.pillarEngine, /There is no search call/);
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

test('a query compiler does not count as a search', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
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
	]) {
		live.set(id, false);
	}
	const input = currentInput(live);
	input.modules = input.modules.filter(
		(name) => name !== 'query' && name !== 'score' && name !== 'stats' && name !== 'search',
	);
	input.hasQuery = false;
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
	assert.match(faq, /Nothing searches/);
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
		analyzers: ['de', 'en'],
		published: false,
		version: '0.0.0',
	};
}
