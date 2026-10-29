// Homepage crate-layer status, read from PROGRESS.md at build time.
// The words in the boxes are a fixed set that fits the ASCII art. The caption
// states the same milestones in sentences, so a later snapshot cannot leave
// "stub" or "planned" on a milestone that has moved.

import { loadProgress } from './progress.mjs';
import { loadStatus } from './status.mjs';

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
 * @param {Milestone} m6
 * @param {Milestone} m7
 */
function engineStatus(m5, m6, m7) {
	const queryStarted = m7.state === 'in progress' || m7.state === 'done' || m7.done > 0;
	const writeStarted = m6.state === 'in progress' || m6.state === 'done' || m6.done > 0;
	if (m5.state === 'done' && m6.state === 'done' && m7.state === 'done') return 'done';
	if (m5.state === 'done' && m6.state === 'done' && m7.state === 'in progress') return 'M7 in progress';
	if (writeStarted && m6.state !== 'done' && !queryStarted) return 'M6, no query';
	if (m6.state === 'done' && !queryStarted) return 'no query yet';
	if (m5.state === 'done' && !writeStarted && !queryStarted) return 'query not started';
	if (m5.state === 'in progress' && !queryStarted) return 'M5 in progress';
	if ((m5.state === 'in progress' || m5.done > 0) && queryStarted) return 'in progress';
	if (untouched(m5) && !queryStarted && !writeStarted) return 'not started';
	return 'in progress';
}

/**
 * @param {Milestone} m6
 * @param {Milestone} m7
 * @param {boolean} walReady
 */
function engineDetail(m6, m7, walReady) {
	const queryStarted = m7.state === 'in progress' || m7.state === 'done' || m7.done > 0;
	const writeStarted = m6.state === 'in progress' || m6.state === 'done' || m6.done > 0;
	if (walReady && queryStarted) return 'WAL and query';
	if (walReady && writeStarted) return 'segments and WAL';
	return 'segments, query';
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
 * @param {string | undefined} engineDetailText
 */
function caption(progress, engineDetailText) {
	const m1 = milestone(progress, 'M1');
	const m2 = milestone(progress, 'M2');
	const m3 = milestone(progress, 'M3');
	const m4 = milestone(progress, 'M4');
	const m5 = milestone(progress, 'M5');
	const m6 = milestone(progress, 'M6');
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
	if (engineDetailText) parts.push(engineDetailText);
	else {
		parts.push(`The segment writer and reader are ${stateClause(m5)}.`);
		parts.push(`The write path is ${stateClause(m6)}.`);
		parts.push(`Query is ${stateClause(m7)}.`);
	}
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
	if (!engineDetailText && m7.state !== 'done') {
		parts.push('Nothing searches, and the public persia crate exports no Db or Collection.');
	} else if (!engineDetailText && m10.state !== 'done') {
		parts.push('The public persia crate exports no Db or Collection.');
	}
	return parts.join(' ');
}

/**
 * @param {Progress} progress
 * @param {{ walReady?: boolean, engineDetail?: string }} [options]
 */
export function layerView(progress, options = {}) {
	const m1 = milestone(progress, 'M1');
	const m2 = milestone(progress, 'M2');
	const m4 = milestone(progress, 'M4');
	const m5 = milestone(progress, 'M5');
	const m6 = milestone(progress, 'M6');
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
		row(engineDetail(m6, m7, options.walReady === true), 'chunks, dedup', 'analyzers'),
		row(engineStatus(m5, m6, m7), narrowStatus(m9), narrowStatus(m4)),
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
		caption: caption(progress, options.engineDetail),
		accent,
		accentAlt,
		serverStatus: serverStatus(m11, m14),
		apiStatus: apiStatus(m10),
		storageDetail,
		storageStatus,
		engineStatus: engineStatus(m5, m6, m7),
		blobStatus: narrowStatus(m9),
		analysisStatus: narrowStatus(m4),
		formatStatus: formatStatus(m1),
	};
}

/** @returns {ReturnType<typeof layerView>} */
export function layerDiagram() {
	const status = loadStatus();
	return layerView(loadProgress(), { walReady: status.walReady, engineDetail: status.engineShort });
}

/**
 * @param {Progress} [progress]
 */
export function writePathCaption(progress = loadProgress()) {
	const live = loadProgress();
	const m6 = milestone(progress, 'M6');
	const liveM6 = milestone(live, 'M6');
	const same =
		m6.state === liveM6.state && m6.done === liveM6.done && m6.leaves === liveM6.leaves;
	if (same) return loadStatus().writePathNow;
	const m2 = milestone(progress, 'M2');
	const m5 = milestone(progress, 'M5');
	return `The container commit is ${stateClause(m2)}. Writing one segment's bytes is ${stateClause(m5)}. The memtable, WAL, flush, and merge path is ${stateClause(m6)}.`;
}

/** Lead-in for the write-path figure. Follows M6 in PROGRESS.md. */
export function writePathLead() {
	return loadStatus().writePathLead;
}

/**
 * Highlight the commit box, and the WAL box once that leaf group is checked.
 * @returns {{ accent: string[], accentAlt: string[] }}
 */
export function writePathAccents() {
	return {
		accent: ['3. commit', 'data, then manifest', 'inactive superblock'],
		accentAlt: loadStatus().walReady ? ['WAL, then memtable'] : [],
	};
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
		loadStatus().engineCalls,
		'SPEC.md wins if a page disagrees with it. PROGRESS.md is the live roadmap counter.',
	].join(' ');
}
