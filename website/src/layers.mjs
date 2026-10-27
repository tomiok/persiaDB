// Homepage crate-layer status, read from PROGRESS.md at build time.
// The words in the boxes are a fixed set that fits the ASCII art. The caption
// states the same milestones in sentences, so a later snapshot cannot leave
// "stub" or "planned" on a milestone that has moved.

import { loadProgress } from './progress.mjs';

/** @typedef {import('./progress.mjs').Progress} Progress */
/** @typedef {import('./progress.mjs').Milestone} Milestone */

const WIDE = 50;
const NARROW = 18;
const LINE = 69;

const FOUNDATION = [
	['M1', 'Format'],
	['M2', 'Local storage'],
	['M3', 'Schema'],
	['M4', 'Text analysis'],
];

/**
 * @param {Progress} progress
 * @param {string} id
 * @returns {Milestone}
 */
function milestone(progress, id) {
	const ms = progress.milestones.find((item) => item.id === id);
	if (!ms) throw new Error(`PROGRESS.md: layer diagram needs ${id}`);
	return ms;
}

/**
 * @param {Milestone} ms
 */
function untouched(ms) {
	return ms.done === 0 && ms.state !== 'in progress' && ms.state !== 'done';
}

/**
 * @param {Milestone} ms
 */
function stateClause(ms) {
	const count = `${ms.done}/${ms.leaves}`;
	if (ms.state === 'done') return `done (${ms.id}, ${count})`;
	if (ms.state === 'in progress') return `in progress (${ms.id}, ${count})`;
	if (ms.state === 'ready') return `not started (${ms.id}; its dependencies are done)`;
	return `not started (${ms.id})`;
}

/**
 * @param {string} text
 * @param {number} width
 * @param {string} kind
 */
function cell(text, width, kind) {
	const inner = ` ${text}`;
	if (inner.length > width) {
		throw new Error(`${kind} cell is ${inner.length} chars, max ${width}: ${JSON.stringify(text)}`);
	}
	return `┃${inner}${' '.repeat(width - inner.length)}┃`;
}

/**
 * @param {string} text
 */
function wide(text) {
	return `║       ${cell(text, WIDE, 'wide')}       ║░`;
}

/**
 * @param {string} left
 * @param {string} middle
 * @param {string} right
 */
function row(left, middle, right) {
	return `║ ${cell(left, NARROW, 'narrow')}  ${cell(middle, NARROW, 'narrow')}  ${cell(right, NARROW, 'narrow')} ║░`;
}

/**
 * @param {Milestone} m11
 * @param {Milestone} m14
 */
function serverStatus(m11, m14) {
	if (m11.state === 'done' && m14.state === 'done') return 'done';
	if (untouched(m11) && untouched(m14)) return 'planned';
	if ((m11.state === 'in progress' || (m11.done > 0 && m11.state !== 'done')) && untouched(m14)) {
		return 'server in progress';
	}
	if (m11.state === 'done' && untouched(m14)) return 'server done · SDKs planned';
	if (untouched(m11) && (m14.state === 'in progress' || m14.done > 0)) return 'SDKs in progress';
	if (untouched(m11) && m14.state === 'done') return 'SDKs done · server planned';
	return 'in progress';
}

/**
 * @param {Milestone} m10
 */
function apiStatus(m10) {
	if (m10.state === 'done') return 'done';
	if (m10.state === 'in progress' || m10.done > 0) return 'in progress';
	// persia/src/lib.rs is module docs until M10 starts. Re-check that file if this label changes.
	return 'stub · no methods yet';
}

/**
 * @param {Milestone} m2
 * @param {Milestone} m12
 * @returns {[string, string]}
 */
function storageLines(m2, m12) {
	if (m2.state === 'done' && m12.state === 'done') {
		return ['memory, file, s3, gcs, azure', 'done'];
	}
	if (m2.state === 'done' && (m12.state === 'in progress' || m12.done > 0)) {
		return ['memory and local file', 'cloud in progress'];
	}
	if (m2.state === 'done' && untouched(m12)) {
		return ['memory and local file', 's3, gcs, azure planned'];
	}
	if (m2.state === 'in progress' || (m2.done > 0 && m2.state !== 'done')) {
		return ['memory and local file', 'in progress'];
	}
	if (untouched(m2) && untouched(m12)) {
		return ['memory, file, s3, gcs, azure', 'planned'];
	}
	return ['memory, file, cloud', 'in progress'];
}

/**
 * @param {Milestone} m1
 */
function formatStatus(m1) {
	if (m1.state === 'done') return 'done';
	if (m1.state === 'in progress' || m1.done > 0) return 'in progress';
	return 'not started';
}

/**
 * @param {Milestone} m5
 * @param {Milestone} m7
 */
function engineStatus(m5, m7) {
	const queryStarted = m7.state === 'in progress' || m7.state === 'done' || m7.done > 0;
	if (m5.state === 'done' && m7.state === 'done') return 'done';
	if (m5.state === 'done' && !queryStarted) return 'query not started';
	if (m5.state === 'done' && m7.state === 'in progress') return 'M7 in progress';
	if (m5.state === 'in progress' && !queryStarted) return 'M5 in progress';
	if ((m5.state === 'in progress' || m5.done > 0) && queryStarted) return 'in progress';
	if (untouched(m5) && !queryStarted) return 'not started';
	return 'in progress';
}

/**
 * @param {Milestone} ms
 */
function narrowStatus(ms) {
	if (ms.state === 'done') return 'done';
	if (ms.state === 'in progress' || ms.done > 0) return 'in progress';
	return 'not started';
}

/**
 * @param {Progress} progress
 */
function caption(progress) {
	const m1 = milestone(progress, 'M1');
	const m2 = milestone(progress, 'M2');
	const m3 = milestone(progress, 'M3');
	const m4 = milestone(progress, 'M4');
	const m5 = milestone(progress, 'M5');
	const m7 = milestone(progress, 'M7');
	const m9 = milestone(progress, 'M9');
	const m10 = milestone(progress, 'M10');
	const m11 = milestone(progress, 'M11');
	const m12 = milestone(progress, 'M12');
	const m14 = milestone(progress, 'M14');

	/** @type {string[]} */
	const parts = [`Status from the ${progress.snapshot} PROGRESS.md snapshot.`];
	const foundation = [m1, m2, m3, m4];
	if (foundation.every((ms) => ms.state === 'done')) {
		parts.push('Format, local storage, schema, and text analysis are done (M1–M4).');
	} else {
		for (const [id, label] of FOUNDATION) {
			parts.push(`${label} is ${stateClause(milestone(progress, id))}.`);
		}
	}
	parts.push(`The segment writer and reader are ${stateClause(m5)}.`);
	parts.push(`Query is ${stateClause(m7)}.`);
	parts.push(`The blob store is ${stateClause(m9)}.`);
	if (untouched(m10)) {
		parts.push(
			`The embedded API has not been started (${m10.id}), and the persia crate exports no Db or Collection.`,
		);
	} else {
		parts.push(`The embedded API is ${stateClause(m10)}.`);
	}
	parts.push(`The server is ${stateClause(m11)}. Network SDKs are ${stateClause(m14)}.`);
	parts.push(`S3, GCS, and Azure are ${stateClause(m12)}.`);
	if (m7.state !== 'done' && m10.state !== 'done') {
		parts.push('Nothing works end to end: there is no indexing API and no query.');
	} else if (m10.state !== 'done') {
		parts.push('Nothing works end to end: there is no indexing API.');
	} else if (m7.state !== 'done') {
		parts.push('Nothing works end to end: there is no query.');
	}
	return parts.join(' ');
}

/**
 * @param {Progress} progress
 */
export function layerView(progress) {
	const m1 = milestone(progress, 'M1');
	const m2 = milestone(progress, 'M2');
	const m4 = milestone(progress, 'M4');
	const m5 = milestone(progress, 'M5');
	const m7 = milestone(progress, 'M7');
	const m9 = milestone(progress, 'M9');
	const m10 = milestone(progress, 'M10');
	const m11 = milestone(progress, 'M11');
	const m12 = milestone(progress, 'M12');
	const m14 = milestone(progress, 'M14');
	const [storageDetail, storageStatus] = storageLines(m2, m12);

	const lines = [
		'╔═ layers ═════════════════════════════════════════════════════════╗ ',
		'║       ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓       ║░',
		wide('persia-server, Rust SDK, Go SDK'),
		wide(serverStatus(m11, m14)),
		'║       ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛       ║░',
		'║                                ▼                                 ║░',
		'║       ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓       ║░',
		wide('persia · Db and Collection'),
		wide(apiStatus(m10)),
		'║       ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛       ║░',
		'║                                ▼                                 ║░',
		'║ ┏━━━━━━━━━━━━━━━━━━┓  ┏━━━━━━━━━━━━━━━━━━┓  ┏━━━━━━━━━━━━━━━━━━┓ ║░',
		row('engine', 'blob', 'analysis'),
		row('segments, query', 'chunks, dedup', 'analyzers'),
		row(engineStatus(m5, m7), narrowStatus(m9), narrowStatus(m4)),
		'║ ┗━━━━━━━━━━━━━━━━━━┛  ┗━━━━━━━━━━━━━━━━━━┛  ┗━━━━━━━━━━━━━━━━━━┛ ║░',
		'║                                ▼                                 ║░',
		'║       ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓       ║░',
		wide('persia-storage'),
		wide(storageDetail),
		wide(storageStatus),
		'║       ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛       ║░',
		'║                                ▼                                 ║░',
		'║       ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓       ║░',
		wide('persia-format'),
		wide('encodings, checksums, codecs'),
		wide(formatStatus(m1)),
		'║       ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛       ║░',
		'╚══════════════════════════════════════════════════════════════════╝░',
		' ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░',
	];
	for (const line of lines) {
		if (line.length !== LINE) {
			throw new Error(`layer diagram line is ${line.length} chars, expected ${LINE}: ${line}`);
		}
	}

	/** @type {string[]} */
	const accent = [];
	/** @type {string[]} */
	const accentAlt = [];
	// Phrases have to be unique to one full-width box. A shared word such as
	// "done" would color the whole middle row, including crates that are not done.
	if (m1.state === 'done' || m1.state === 'in progress' || m1.done > 0) {
		accent.push('persia-format', 'encodings, checksums');
	}
	if (m2.state === 'done' && m12.state === 'done') accent.push('persia-storage');
	else if (m2.state === 'done' || m2.state === 'in progress' || m2.done > 0) accentAlt.push('persia-storage');

	return {
		diagram: lines.join('\n'),
		caption: caption(progress),
		accent,
		accentAlt,
		serverStatus: serverStatus(m11, m14),
		apiStatus: apiStatus(m10),
		storageDetail,
		storageStatus,
		engineStatus: engineStatus(m5, m7),
		blobStatus: narrowStatus(m9),
		analysisStatus: narrowStatus(m4),
		formatStatus: formatStatus(m1),
	};
}

/** @returns {ReturnType<typeof layerView>} */
export function layerDiagram() {
	return layerView(loadProgress());
}

/**
 * @param {Progress} [progress]
 */
export function writePathCaption(progress = loadProgress()) {
	const m2 = milestone(progress, 'M2');
	const m5 = milestone(progress, 'M5');
	const m6 = milestone(progress, 'M6');
	return `The container commit is ${stateClause(m2)} and does not index documents. This memtable, WAL, flush, and merge path is ${stateClause(m6)}. Writing one segment's bytes is ${stateClause(m5)} and is not this path.`;
}

/**
 * @param {Progress} [progress]
 */
export function queryPathCaption(progress = loadProgress()) {
	const m4 = milestone(progress, 'M4');
	const m7 = milestone(progress, 'M7');
	const m8 = milestone(progress, 'M8');
	return `Text analysis is ${stateClause(m4)}. Searching segments is ${stateClause(m7)}. Zero-results relaxation is ${stateClause(m8)}.`;
}

/**
 * @param {Progress} [progress]
 */
export function storageDiagramCaption(progress = loadProgress()) {
	const m1 = milestone(progress, 'M1');
	const m2 = milestone(progress, 'M2');
	const m12 = milestone(progress, 'M12');
	return `Block framing is ${stateClause(m1)}. Memory and local-file backends are ${stateClause(m2)}. S3, GCS, and Azure are ${stateClause(m12)}.`;
}

/** Paragraph under the llms.txt title. Milestone wording comes from PROGRESS.md. */
export function llmsStatusParagraph() {
	const view = layerDiagram();
	return [
		'Pre-alpha.',
		view.caption,
		'`Db::open`, `put`, `commit`, and `search` in these pages are illustrative, from SPEC.md, and are not in the crate.',
		'SPEC.md wins if a page disagrees with it. PROGRESS.md is the live roadmap counter.',
	].join(' ');
}
