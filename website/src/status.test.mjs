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
	assert.equal(m7?.done, 0);
	assert.equal(status.walReady, true);
	for (const text of [status.engineShort, status.engineNow, status.libraryNow, status.writePathNow]) {
		assert.match(text, /WAL with chunks, group commit, and replay/);
		assert.match(text, /no query engine|Nothing searches/);
		assert.equal(text.includes('No WAL'), false);
		assert.equal(text.includes('no memtable'), false);
		assert.equal(text.includes('M5 in progress'), false);
		assert.equal(text.includes('"'), false);
	}
	assert.match(status.preAlpha, /Nothing runs a query/);
	assert.match(status.preAlpha, /can put, delete, and commit/);
	assert.equal(status.preAlpha.includes('"'), false);
	assert.match(status.engineShort, /tombstone sidecars/);
	assert.match(status.engineShort, /segment merging/);
	assert.match(status.engineShort, /Open and recovery orchestration/);
	assert.match(status.engineNow, /Public modules: .*wal/);
	assert.equal(status.engineNow.includes('query'), true);
	assert.equal(status.engineModules.split(', ').includes('query'), false);
	assert.match(status.engineCalls, /Database has open, create_collection, put, put_batch, delete, and commit/);
	assert.match(status.engineCalls, /no persia::Db/);
	assert.match(status.engineCalls, /no search/);
	assert.match(status.writePathNow, /replays the WAL/);
	assert.match(status.writePathNow, /Nothing searches/);
	assert.match(status.noPublicApi, /exports no Db and no Collection/);
	assert.match(status.noPublicApi, /unpublished/);
	assert.match(status.m10Blocked, /M6 \(in progress\)/);
	assert.match(status.m10Blocked, /M7 \(not started\)/);
	assert.match(status.m5Cell, /Done \(M5,/);
	assert.match(status.pillarEngine, /WAL, commit, and segment merging/);
	assert.match(status.pillarEngine, /Scoring and query execution do not/);
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

test('a query module stops the no-query claim', () => {
	const live = parseRoadmapLeaves(readFileSync(path.join(root, 'ROADMAP.md'), 'utf8'));
	const input = currentInput(live);
	input.modules = [...input.modules, 'query'];
	input.hasQuery = true;
	const progressCopy = structuredClone(input.progress);
	const m7 = progressCopy.milestones.find((ms) => ms.id === 'M7');
	if (!m7) throw new Error('missing M7');
	m7.state = 'in progress';
	m7.done = 1;
	input.progress = progressCopy;
	const next = buildStatus(input);
	assert.match(next.engineShort, /query module is in the tree/);
	assert.equal(next.engineShort.includes('There is no query engine'), false);
	assert.equal(next.engineShort.includes('Nothing searches'), false);
});

test('public modules follow lib.rs order', () => {
	const mods = publicModules(readFileSync(path.join(root, 'crates/persia-engine/src/lib.rs'), 'utf8'));
	assert.deepEqual(mods, ['db', 'document', 'manifest', 'memtable', 'merge', 'schema', 'segment', 'tombstone', 'view', 'wal']);
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
		modules: ['db', 'document', 'manifest', 'memtable', 'merge', 'schema', 'segment', 'tombstone', 'view', 'wal'],
		dbFns: new Set(['open', 'create_collection', 'put', 'put_batch', 'delete', 'commit']),
		snapshotFns: new Set(['get']),
		persiaExports: false,
		blobExports: false,
		hasOpenOptions: false,
		hasPutStruct: false,
		hasPutBlob: false,
		hasGetBlob: false,
		hasQuery: false,
		analyzers: ['de', 'en'],
		published: false,
		version: '0.0.0',
	};
}
