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

test('the live snapshot fills every box and drops the stale labels', () => {
	const parsed = parseProgress(progressMd);
	const view = layerDiagram();
	const lines = view.diagram.split('\n');
	assert.equal(new Set(lines.map((line) => line.length)).size, 1);
	assert.equal(view.formatStatus, 'done');
	assert.equal(view.analysisStatus, 'done');
	assert.equal(view.storageDetail, 'memory and local file');
	assert.equal(view.storageStatus, 's3, gcs, azure planned');
	assert.equal(view.engineStatus, 'in progress');
	assert.equal(view.diagram.includes('WAL and query'), true);
	assert.equal(view.diagram.includes('M5 in progress'), false);
	assert.equal(view.blobStatus, 'not started');
	assert.equal(view.apiStatus, 'stub · no methods yet');
	assert.equal(view.serverStatus, 'planned');
	assert.equal(view.diagram.includes('stub · M4'), false);
	assert.equal(view.diagram.includes('in progress · code exists'), false);
	assert.equal(view.diagram.includes('workspace stubs'), false);
	assert.match(view.caption, /Format, local storage, schema, and text analysis are done \(M1–M4\)/);
	const m5 = parsed.milestones.find((ms) => ms.id === 'M5');
	const m6 = parsed.milestones.find((ms) => ms.id === 'M6');
	assert.equal(m5?.state, 'done');
	assert.equal(m6?.state, 'in progress');
	assert.match(view.caption, /WAL with chunks, group commit, and replay/);
	assert.match(view.caption, /query path is in progress/);
	assert.match(view.caption, /Nothing searches/);
	assert.equal(view.caption.includes('no query engine'), false);
	assert.match(view.caption, /exports no Db or Collection/);
	assert.equal(view.caption.includes('M5 in progress'), false);
	assert.equal(view.caption.includes('no indexing API'), false);
	assert.equal(view.caption.includes('only persia-format'), false);
	assert.deepEqual(view.accent, ['persia-format', 'encodings, checksums']);
	assert.deepEqual(view.accentAlt, ['persia-storage']);
	assert.equal(layerDiagram().caption, view.caption);
	assert.equal(layerView(loadProgress()).engineStatus, 'in progress');
});

test('figure captions follow the same snapshot', () => {
	const parsed = parseProgress(progressMd);
	const m5 = parsed.milestones.find((ms) => ms.id === 'M5');
	const m6 = parsed.milestones.find((ms) => ms.id === 'M6');
	assert.match(writePathCaption(parsed), /container commit is done \(M2,/);
	assert.match(writePathCaption(parsed), new RegExp(`in progress \\(M6, ${m6?.done}/${m6?.leaves}\\)`));
	assert.match(writePathCaption(parsed), /WAL with chunks, group commit, and replay/);
	assert.match(writePathCaption(parsed), /Nothing searches/);
	assert.equal(writePathCaption(parsed).includes('not started (M6)'), false);
	assert.match(queryPathCaption(parsed), /Text analysis is done \(M4,/);
	assert.match(queryPathCaption(parsed), /The query milestone is in progress \(M7/);
	assert.match(queryPathCaption(parsed), /Zero-results relaxation is not started \(M8\)/);
	assert.match(storageDiagramCaption(parsed), /Block framing is done \(M1,/);
	assert.match(storageDiagramCaption(parsed), /S3, GCS, and Azure are not started \(M12\)/);
	const llms = llmsStatusParagraph();
	assert.match(llms, /Pre-alpha\./);
	assert.match(llms, /WAL with chunks/);
	assert.match(llms, /no search/);
	assert.equal(llms.includes('indexes a document, or runs a search'), false);
	assert.equal(llms.includes('No WAL'), false);
	assert.equal(m5?.state, 'done');
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
