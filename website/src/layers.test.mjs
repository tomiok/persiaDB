import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
	layerDiagram,
	layerView,
	llmsStatusParagraph,
	queryPathCaption,
	storageDiagramCaption,
	writePathCaption,
} from './layers.mjs';
import { loadProgress, parseProgress } from './progress.mjs';

const progressMd = readFileSync(
	path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../PROGRESS.md'),
	'utf8',
);

/**
 * @param {Partial<import('./progress.mjs').Milestone> & { id: string, state: import('./progress.mjs').Milestone['state'], done: number, leaves: number }} fields
 */
function ms(fields) {
	return {
		name: fields.id,
		doing: 0,
		deps: '—',
		delivers: fields.id,
		...fields,
	};
}

/**
 * @param {Array<ReturnType<typeof ms>>} milestones
 * @param {Partial<import('./progress.mjs').Progress>} [extra]
 */
function progress(milestones, extra = {}) {
	const done = milestones.reduce((sum, item) => sum + item.done, 0);
	const total = milestones.reduce((sum, item) => sum + item.leaves, 0);
	return {
		done,
		total,
		percent: total === 0 ? 0 : Math.round((100 * done) / total),
		inProgress: 0,
		snapshot: '2026-01-01',
		milestones,
		...extra,
	};
}

const ids = ['M1', 'M2', 'M4', 'M5', 'M7', 'M9', 'M10', 'M11', 'M12', 'M14', 'M3', 'M6', 'M8'];

// The two tests below read the live PROGRESS.md, so what they expect follows its states rather than naming them: a
// milestone finished today can reopen when a leaf is added to it (ROADMAP 10.1.52 found these two pinned to an old
// snapshot, M6 and M9 in progress and no Db).

/**
 * The word a status cell or caption uses for a milestone, as the synthetic tests below pin it.
 * @param {import('./progress.mjs').Milestone} ms
 */
function stateWord(ms) {
	if (ms.state === 'done') return 'done';
	if (ms.state === 'in progress' || ms.done > 0) return 'in progress';
	return 'not started';
}

/**
 * @param {import('./progress.mjs').Milestone} ms
 */
function started(ms) {
	return ms.done > 0 || ms.state === 'in progress' || ms.state === 'done';
}

/**
 * @param {import('./progress.mjs').Progress} parsed
 * @param {string} id
 */
function live(parsed, id) {
	const found = parsed.milestones.find((item) => item.id === id);
	assert.ok(found, `PROGRESS.md has no ${id}`);
	return found;
}

test('the live snapshot fills every box and drops the stale labels', () => {
	const parsed = parseProgress(progressMd);
	const [m1, m2, m3, m4, m5, m6, m7, m9, m10, m11, m12, m14] = [
		'M1', 'M2', 'M3', 'M4', 'M5', 'M6', 'M7', 'M9', 'M10', 'M11', 'M12', 'M14',
	].map((id) => live(parsed, id));
	const view = layerDiagram();
	const lines = view.diagram.split('\n');
	assert.equal(new Set(lines.map((line) => line.length)).size, 1);
	assert.equal(view.formatStatus, stateWord(m1));
	assert.equal(view.analysisStatus, stateWord(m4));
	assert.equal(view.blobStatus, stateWord(m9));
	assert.equal(view.apiStatus, started(m10) ? stateWord(m10) : 'stub · no methods yet');
	assert.equal(view.engineStatus === 'done', [m5, m6, m7].every((ms) => ms.state === 'done'));
	if (m2.state === 'done') assert.equal(view.storageDetail, 'memory and local file');
	if (!started(m12)) assert.equal(view.storageStatus, 's3, gcs, azure planned');
	if (!started(m11) && !started(m14)) assert.equal(view.serverStatus, 'planned');
	assert.equal(view.diagram.includes('WAL and query'), started(m6) && started(m7));
	if (m5.state === 'done') assert.equal(view.diagram.includes('M5 in progress'), false);
	assert.equal(view.diagram.includes('stub · M4'), false);
	assert.equal(view.diagram.includes('in progress · code exists'), false);
	assert.equal(view.diagram.includes('workspace stubs'), false);
	if ([m1, m2, m3, m4].every((ms) => ms.state === 'done')) {
		assert.match(view.caption, /Format, local storage, schema, and text analysis are done \(M1–M4\)/);
	}
	assert.match(view.caption, /WAL with chunks, group commit, and replay/);
	for (const ms of [m9, m10]) {
		if (started(ms)) assert.ok(view.caption.includes(`${stateWord(ms)} (${ms.id}, ${ms.done}/${ms.leaves})`), ms.id);
	}
	assert.equal(view.caption.includes('exports no Db or Collection'), !started(m10));
	if (m7.state === 'done') {
		assert.equal(view.caption.includes('query path is in progress'), false);
		assert.equal(view.caption.includes('no query engine'), false);
	}
	if (m5.state === 'done') assert.equal(view.caption.includes('M5 in progress'), false);
	assert.equal(view.caption.includes('no indexing API'), false);
	assert.equal(view.caption.includes('only persia-format'), false);
	assert.deepEqual(view.accent, ['persia-format', 'encodings, checksums']);
	assert.deepEqual(view.accentAlt, ['persia-storage']);
	assert.equal(layerDiagram().caption, view.caption);
	assert.equal(layerView(loadProgress()).engineStatus, view.engineStatus);
});

test('figure captions follow the same snapshot', () => {
	const parsed = parseProgress(progressMd);
	const [m1, m2, m4, m6, m7, m8, m12] = ['M1', 'M2', 'M4', 'M6', 'M7', 'M8', 'M12'].map((id) => live(parsed, id));
	const write = writePathCaption(parsed);
	assert.match(write, new RegExp(`container commit is ${stateWord(m2)} \\(M2,`));
	assert.ok(write.includes(`${stateWord(m6)} (M6, ${m6.done}/${m6.leaves})`), write);
	assert.match(write, /WAL with chunks, group commit, and replay/);
	assert.equal(write.includes('A collection snapshot can be searched as library code'), false);
	if (started(m6)) assert.equal(write.includes('not started (M6)'), false);
	const query = queryPathCaption(parsed);
	assert.ok(query.includes(`Text analysis is ${stateWord(m4)} (M4,`), query);
	assert.ok(query.includes(`The query milestone is ${stateWord(m7)} (M7`), query);
	if (m7.state === 'done') assert.match(query, /through Collection::search/);
	if (started(m8)) assert.equal(query.includes('not started (M8)'), false);
	const storage = storageDiagramCaption(parsed);
	assert.ok(storage.includes(`Block framing is ${stateWord(m1)} (M1,`), storage);
	if (!started(m12)) assert.match(storage, /S3, GCS, and Azure are not started \(M12\)/);
	const llms = llmsStatusParagraph();
	assert.match(llms, /^Pre-alpha\./);
	assert.match(llms, /WAL with chunks/);
	assert.ok(llms.includes(layerDiagram().caption), 'llms.txt repeats the layer caption');
	assert.match(llms, /persia::Db \(/);
	assert.match(llms, /persia::Collection \(/);
	assert.equal(llms.includes('CollectionViews'), false);
	assert.equal(llms.includes('indexes a document, or runs a search'), false);
	assert.equal(llms.includes('No WAL'), false);
});

test('a finished segment milestone says query has not started', () => {
	const view = layerView(
		progress(
			ids.map((id) => {
				if (id === 'M5') return ms({ id, state: 'done', done: 43, leaves: 43 });
				if (id === 'M1' || id === 'M4') return ms({ id, state: 'done', done: 1, leaves: 1 });
				return ms({ id, state: 'blocked', done: 0, leaves: 1 });
			}),
		),
	);
	assert.equal(view.engineStatus, 'query not started');
	assert.equal(view.caption.includes('M1–M4'), false);
	assert.match(view.caption, /Format is done \(M1, 1\/1\)/);
});

test('an unfinished format milestone is not labeled done', () => {
	const view = layerView(
		progress(
			ids.map((id) => {
				if (id === 'M1') return ms({ id, state: 'in progress', done: 10, leaves: 53 });
				if (id === 'M4') return ms({ id, state: 'in progress', done: 2, leaves: 29 });
				if (id === 'M2') return ms({ id, state: 'blocked', done: 0, leaves: 46 });
				return ms({ id, state: 'blocked', done: 0, leaves: 1 });
			}),
		),
	);
	assert.equal(view.formatStatus, 'in progress');
	assert.equal(view.analysisStatus, 'in progress');
	assert.equal(view.storageStatus, 'planned');
	assert.equal(view.engineStatus, 'not started');
	assert.deepEqual(view.accent, ['persia-format', 'encodings, checksums']);
	assert.deepEqual(view.accentAlt, []);
	assert.match(view.caption, /Format is in progress \(M1, 10\/53\)/);
});
