// What the crates implement today.
//
// Milestone counts and done / in progress / ready / blocked come from PROGRESS.md.
// Which write-path pieces are finished comes from ROADMAP.md checkboxes.
// Module and function names come from the crate sources.
// Pages share the sentences below (%%TOKENS%%) instead of copying a crate list.
// A claim is emitted only when the leaves that define it are checked and the
// matching code is still in the tree. An unchecked follow-up (open/recovery
// orchestration, query) stays unclaimed.

import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { PROGRESS_TOKEN, loadProgress, progressSentence } from './progress.mjs';

/**
 * @typedef {import('./progress.mjs').Progress} Progress
 * @typedef {import('./progress.mjs').Milestone} Milestone
 */

/**
 * Leaves that have to be checked before the matching sentence is fair.
 * Follow-ups that are still open (6.1.5, 6.3.10, 6.4.5, 6.5.6, 6.6.*) are
 * not in these lists, so a finished core is not held back by them, and an
 * unchecked core leaf drops the claim.
 * @type {Record<string, { leaves: string[], code: (ctx: StatusInput) => boolean, done: string, partial: string }>}
 */
const GROUPS = {
	memtable: {
		leaves: ['6.1.1', '6.1.2', '6.1.3', '6.1.4'],
		code: (ctx) => ctx.modules.includes('memtable'),
		done: 'a memtable',
		partial: 'a memtable (not every leaf is done)',
	},
	wal: {
		leaves: ['6.2.1', '6.2.2', '6.2.3', '6.2.4.1', '6.2.5'],
		code: (ctx) => ctx.modules.includes('wal'),
		done: 'a WAL with chunks, group commit, and replay',
		partial: 'a WAL (not every leaf is done)',
	},
	commit: {
		leaves: ['6.3.1', '6.3.2', '6.3.3', '6.3.4', '6.3.7'],
		code: (ctx) => ctx.modules.includes('db') && ctx.dbFns.has('commit'),
		done: 'flush, commit, snapshots, auto-commit, and open checks',
		partial: 'flush and commit (not every leaf is done)',
	},
	upserts: {
		leaves: ['6.4.1', '6.4.2', '6.4.3'],
		code: (ctx) => ctx.modules.includes('tombstone') && ctx.dbFns.has('delete'),
		done: 'deletes and upserts across segments with tombstone sidecars',
		partial: 'deletes and upserts (not every leaf is done)',
	},
	merge: {
		leaves: ['6.5.1', '6.5.2.1', '6.5.2.2', '6.5.2.3', '6.5.3', '6.5.4'],
		code: (ctx) => ctx.modules.includes('merge'),
		done: 'segment merging (a merge policy, a segment merger, a background scheduler, and deletion of obsolete objects)',
		partial: 'segment merging (not every leaf is done)',
	},
};

/** Section 6.6. Not part of the write-path claim until every leaf is checked. */
const OPEN_LEAVES = ['6.6.1', '6.6.2', '6.6.3', '6.6.4', '6.6.5'];

const CALLS = ['open', 'create_collection', 'put', 'put_batch', 'delete', 'commit'];

/**
 * @typedef {Object} StatusInput
 * @property {Progress} progress
 * @property {Map<string, boolean>} leaves
 * @property {string[]} modules
 * @property {Set<string>} dbFns
 * @property {Set<string>} snapshotFns
 * @property {boolean} persiaExports
 * @property {boolean} blobExports
 * @property {boolean} hasOpenOptions
 * @property {boolean} hasPutStruct
 * @property {boolean} hasPutBlob
 * @property {boolean} hasGetBlob
 * @property {boolean} hasQuery
 * @property {string[]} analyzers
 * @property {boolean} published
 * @property {string} version
 */

/**
 * @typedef {Object} StatusText
 * @property {string} engineShort
 * @property {string} engineNow
 * @property {string} engineModules
 * @property {string} libraryNow
 * @property {string} writePathNow
 * @property {string} durabilityCell
 * @property {string} engineCalls
 * @property {string} noPublicApi
 * @property {string} preAlpha
 * @property {string} m10Blocked
 * @property {string} m5Cell
 * @property {string} pillarEngine
 * @property {string} pillarCorrectness
 * @property {string} pillarCloud
 * @property {boolean} walReady
 * @property {string} writePathLead
 * @property {Record<string, string>} tokens
 */

/** @type {StatusText | undefined} */
let cached;

/** @returns {string} */
function repoRoot() {
	const fromHere = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
	if (existsSync(path.join(fromHere, 'ROADMAP.md'))) return fromHere;
	const fromCwd = path.resolve(process.cwd(), '..');
	if (existsSync(path.join(fromCwd, 'ROADMAP.md'))) return fromCwd;
	throw new Error('ROADMAP.md not found');
}

/**
 * @param {string} text
 * @returns {Map<string, boolean>}
 */
export function parseRoadmapLeaves(text) {
	/** @type {Map<string, boolean>} */
	const leaves = new Map();
	for (const match of text.matchAll(/^[ \t]*- \[([ x~])\] (\d+(?:\.\d+)+)\b/gm)) {
		const id = match[2];
		if (id === undefined || match[1] === undefined) continue;
		if (leaves.has(id)) throw new Error(`ROADMAP.md: duplicate leaf ${id}`);
		leaves.set(id, match[1] === 'x');
	}
	if (leaves.size === 0) throw new Error('ROADMAP.md: no leaves parsed');
	return leaves;
}

/**
 * @param {string} libRs
 * @returns {string[]}
 */
export function publicModules(libRs) {
	return [...libRs.matchAll(/^pub mod ([A-Za-z0-9_]+);/gm)].map((match) => match[1] ?? '');
}

/**
 * @param {string} source
 * @returns {Set<string>}
 */
export function exportedFns(source) {
	return new Set([...source.matchAll(/^\s*pub fn ([A-Za-z0-9_]+)\b/gm)].map((match) => match[1] ?? ''));
}

/**
 * @param {string} registryRs
 * @returns {string[]}
 */
export function builtinAnalyzerNames(registryRs) {
	const block = registryRs.match(/const BUILTIN_ANALYZERS[\s\S]*?\n\];/);
	if (!block) throw new Error('persia-analysis: BUILTIN_ANALYZERS not found');
	const names = [...block[0].matchAll(/name: "([^"]+)"/g)].map((match) => match[1] ?? '');
	if (names.length === 0) throw new Error('persia-analysis: no built-in analyzer names');
	return names;
}

/**
 * @param {string} cargo
 * @returns {{ published: boolean, version: string }}
 */
export function workspacePackage(cargo) {
	const block = cargo.match(/\[workspace\.package\][^\[]*/);
	if (!block) throw new Error('Cargo.toml: no [workspace.package]');
	const publish = block[0].match(/^publish\s*=\s*(true|false)\s*$/m);
	const version = block[0].match(/^version\s*=\s*"([^"]+)"\s*$/m);
	if (!publish?.[1] || !version?.[1]) {
		throw new Error('Cargo.toml: [workspace.package] is missing publish or version');
	}
	return { published: publish[1] === 'true', version: version[1] };
}

/**
 * @param {string[]} items
 */
function englishList(items) {
	if (items.length === 0) return '';
	if (items.length === 1) return items[0] ?? '';
	if (items.length === 2) return `${items[0]} and ${items[1]}`;
	return `${items.slice(0, -1).join(', ')}, and ${items[items.length - 1]}`;
}

/**
 * Fits after "is": "done (M2, 46/46)".
 * @param {Milestone} ms
 */
function isState(ms) {
	const count = `${ms.done}/${ms.leaves}`;
	if (ms.state === 'done') return `done (${ms.id}, ${count})`;
	if (ms.state === 'in progress') return `in progress (${ms.id}, ${count})`;
	if (ms.state === 'ready') return `not started (${ms.id}; its dependencies are done)`;
	return `not started (${ms.id})`;
}

/**
 * Fits in parentheses: "M3 done, 15/15".
 * @param {Milestone} ms
 */
function countState(ms) {
	const count = `${ms.done}/${ms.leaves}`;
	if (ms.state === 'done') return `${ms.id} done, ${count}`;
	if (ms.state === 'in progress') return `${ms.id} in progress, ${count}`;
	if (ms.state === 'ready') return `${ms.id} not started; its dependencies are done`;
	return `${ms.id} not started`;
}

/**
 * @param {Progress} progress
 * @param {string} id
 * @returns {Milestone}
 */
function milestone(progress, id) {
	const ms = progress.milestones.find((item) => item.id === id);
	if (!ms) throw new Error(`PROGRESS.md: status text needs ${id}`);
	return ms;
}

/**
 * @param {Map<string, boolean>} leaves
 * @param {string[]} ids
 * @returns {'done' | 'partial' | 'todo'}
 */
function groupState(leaves, ids) {
	const flags = ids.map((id) => {
		if (!leaves.has(id)) throw new Error(`ROADMAP.md: missing leaf ${id}`);
		return leaves.get(id) === true;
	});
	if (flags.every(Boolean)) return 'done';
	if (flags.some(Boolean)) return 'partial';
	return 'todo';
}

/**
 * @param {StatusInput} ctx
 * @param {keyof typeof GROUPS} id
 */
function groupPhrase(ctx, id) {
	const group = GROUPS[id];
	const state = groupState(ctx.leaves, group.leaves);
	if (state === 'todo') return null;
	if (!group.code(ctx)) {
		if (state === 'done') {
			throw new Error(`ROADMAP.md marks ${id} done but the matching code is not in the tree`);
		}
		return null;
	}
	return state === 'done' ? group.done : group.partial;
}

/**
 * @param {string} text
 */
function seal(text) {
	const flat = text.replace(/ +/g, ' ').trim();
	if (flat.includes('%%') || flat.includes('"') || flat.includes('\n')) {
		throw new Error(`status sentence is not safe to embed: ${flat.slice(0, 120)}`);
	}
	return flat;
}

/**
 * @param {Milestone} ms
 * @param {string} noun
 */
/**
 * @param {Milestone} ms
 * @param {string} noun
 * @param {boolean} [plural]
 */
function startedPhrase(ms, noun, plural = false) {
	const count = `${ms.done}/${ms.leaves}`;
	const be = plural ? 'are' : 'is';
	const have = plural ? 'have' : 'has';
	if (ms.state === 'done') return `${noun} ${be} done (${ms.id}, ${count})`;
	if (ms.state === 'in progress' || ms.done > 0) return `${noun} ${be} in progress (${ms.id}, ${count})`;
	return `${noun} ${have} not been started (${ms.id})`;
}

/**
 * @param {StatusInput} ctx
 * @returns {StatusText}
 */
export function buildStatus(ctx) {
	const m1 = milestone(ctx.progress, 'M1');
	const m2 = milestone(ctx.progress, 'M2');
	const m3 = milestone(ctx.progress, 'M3');
	const m4 = milestone(ctx.progress, 'M4');
	const m5 = milestone(ctx.progress, 'M5');
	const m6 = milestone(ctx.progress, 'M6');
	const m7 = milestone(ctx.progress, 'M7');
	const m8 = milestone(ctx.progress, 'M8');
	const m9 = milestone(ctx.progress, 'M9');
	const m10 = milestone(ctx.progress, 'M10');
	const m12 = milestone(ctx.progress, 'M12');
	const m13 = milestone(ctx.progress, 'M13');

	const memtable = groupPhrase(ctx, 'memtable');
	const wal = groupPhrase(ctx, 'wal');
	const commit = groupPhrase(ctx, 'commit');
	const upserts = groupPhrase(ctx, 'upserts');
	const merge = groupPhrase(ctx, 'merge');
	const writePieces = [memtable, wal, commit, upserts, merge].filter((item) => item !== null);
	const openState = groupState(ctx.leaves, OPEN_LEAVES);
	const walReady = groupState(ctx.leaves, GROUPS.wal.leaves) === 'done' && ctx.modules.includes('wal');

	/** @type {string[]} */
	const engineBits = [`collection schema, documents, and manifests (${countState(m3)})`];
	if (m5.state === 'done' || m5.done > 0) {
		engineBits.push(`a segment builder, writer, and reader (${countState(m5)})`);
	}
	let writeSentence;
	if (writePieces.length > 0) {
		writeSentence = `Its write path is ${isState(m6)}: ${englishList(writePieces)}.`;
	} else {
		writeSentence = `Its write path is ${isState(m6)}.`;
	}
	/** @type {string[]} */
	const gaps = [];
	if ((m6.state === 'in progress' || m6.state === 'done' || m6.done > 0) && openState !== 'done') {
		gaps.push(
			openState === 'partial'
				? 'Open and recovery orchestration is only partly done.'
				: 'Open and recovery orchestration (single-writer enforcement, persia verify, and the crash-loop harness) is not done.',
		);
	}
	if (m7.state === 'done') {
		// Query milestone is finished.
	} else if (ctx.hasQuery) {
		gaps.push(`A query module is in the tree. The query milestone is ${isState(m7)}.`);
	} else if (m7.done === 0 && m7.state !== 'in progress') {
		gaps.push(`There is no query engine (${countState(m7)}).`);
	} else {
		gaps.push(`The query engine is ${isState(m7)}.`);
	}
	if (m8.state !== 'done') {
		gaps.push(
			m8.done === 0 && m8.state !== 'in progress'
				? `Relaxation has not been started (${m8.id}).`
				: `Relaxation is ${isState(m8)}.`,
		);
	}
	if (m7.state !== 'done' && !ctx.hasQuery) gaps.push('Nothing searches.');

	const engineShort = seal(
		`persia-engine implements ${englishList(engineBits)}. ${writeSentence} ${gaps.join(' ')}`,
	);

	const engineModules = seal(englishList(ctx.modules));
	const engineNow = seal(`${engineShort} Public modules: ${engineModules}.`);

	const cloud =
		m12.state === 'done'
			? 'S3, GCS, and Azure are implemented.'
			: `${startedPhrase(m12, 'S3, GCS, and Azure', true)}.`;
	const blob = ctx.blobExports
		? `persia-blob exports an API (${countState(m9)}).`
		: `persia-blob exports no API (${countState(m9)}).`;
	const persia = ctx.persiaExports
		? 'The persia crate exports a public API.'
		: 'The public persia crate exports no Db and no Collection.';
	const unpublished = ctx.published ? '' : ` The crates are unpublished (publish = false, version ${ctx.version}).`;
	const libraryNow = seal(
		`persia-format implements the on-disk codecs (${countState(m1)}). persia-storage implements the Storage trait, memory and local-file backends, and the single-file container (${countState(m2)}). ${cloud} ${engineShort} persia-analysis implements tokenizers and the built-in analyzers ${englishList(ctx.analyzers)} (${countState(m4)}). It does not search. ${blob} ${persia}${unpublished}`,
	);

	const replayReady =
		walReady &&
		groupState(ctx.leaves, GROUPS.commit.leaves) === 'done' &&
		ctx.dbFns.has('open') &&
		ctx.dbFns.has('put') &&
		ctx.dbFns.has('commit');
	const openSentence = replayReady
		? 'Database::open reloads the committed manifest, checks it against the superblock, and replays the WAL.'
		: '';
	const mentionOpen = m6.state === 'in progress' || m6.state === 'done' || m6.done > 0;
	const openGap = !mentionOpen
		? ''
		: openState === 'done'
			? 'Open and recovery orchestration is done.'
			: 'Open and recovery orchestration (single-writer enforcement, persia verify, and the crash-loop harness) is not done.';
	const searchGap = m7.state === 'done' || ctx.hasQuery ? '' : 'Nothing searches.';
	const writePathNow = seal(
		`The container commit is ${isState(m2)} and is crash-tested. ${writeSentence.replace(/^Its /, 'The engine ')} ${openSentence} ${openGap} ${searchGap}`,
	);

	/** @type {string[]} */
	const durableBits = [];
	if (wal) durableBits.push('WAL');
	if (commit) durableBits.push('flush, commit, and snapshots');
	if (upserts) durableBits.push('deletes and upserts');
	if (merge) durableBits.push('segment merging');
	const durableDetail =
		durableBits.length > 0 ? `: ${englishList(durableBits)}` : '';
	const durabilityCell = seal(
		`Container commit is ${isState(m2)}. The document write path is ${isState(m6)}${durableDetail}. ${openGap} ${searchGap}`,
	);

	const present = CALLS.filter((name) => ctx.dbFns.has(name));
	const hasSentence =
		present.length > 0
			? `persia_engine::db::Database has ${englishList(present)}.`
			: 'persia_engine::db::Database does not export open, put, or commit.';
	const getSentence = ctx.snapshotFns.has('get')
		? ' CollectionSnapshot::get fetches one document by id.'
		: '';
	/** @type {string[]} */
	const absent = [];
	if (!ctx.persiaExports) absent.push('persia::Db');
	if (!ctx.hasQuery) absent.push('search');
	if (!ctx.hasPutStruct) absent.push('put_struct');
	if (!ctx.hasPutBlob) absent.push('put_blob');
	if (!ctx.hasGetBlob) absent.push('get_blob');
	if (!ctx.hasOpenOptions) absent.push('OpenOptions');
	const absentSentence =
		absent.length > 0 ? ` There is ${englishList(absent.map((name) => `no ${name}`))}.` : '';
	const engineCalls = seal(
		`${hasSentence}${getSentence}${absentSentence} Those SPEC names are not the public embedded API.`,
	);

	const noPublicApi = seal(
		ctx.persiaExports
			? 'The persia crate exports a public API.'
			: `The public persia crate exports no Db and no Collection.${m7.state === 'done' || ctx.hasQuery ? '' : ' Nothing runs a query.'}${unpublished}`,
	);

	const canWrite = ctx.dbFns.has('put') && ctx.dbFns.has('delete') && ctx.dbFns.has('commit');
	const preAlpha = seal(
		`Persia DB is pre-alpha. ${ctx.persiaExports ? 'The persia crate exports a public API.' : 'The public persia crate has no Db::open.'} ${canWrite ? 'persia-engine can put, delete, and commit a document.' : 'The engine does not yet put documents.'} ${m7.state === 'done' || ctx.hasQuery ? 'A query engine is in the tree.' : 'Nothing runs a query.'}`,
	);

	const blocking = ['M6', 'M7', 'M8', 'M9']
		.map((id) => milestone(ctx.progress, id))
		.filter((ms) => ms.state !== 'done');
	const m10Blocked = seal(
		m10.state === 'done'
			? 'The embedded API milestone (M10) is done.'
			: blocking.length === 0
				? `The embedded API is ${isState(m10)}.`
				: `The first release a caller is supposed to use is v0.1 (M10). That milestone is blocked on ${englishList(blocking.map((ms) => `${ms.id} (${ms.state === 'in progress' ? 'in progress' : 'not started'})`))}.`,
	);

	const m5Cell = seal(
		m5.state === 'done'
			? `Done (${m5.id}, ${m5.done}/${m5.leaves}). Nothing searches the segment.`
			: `${countState(m5)}. Nothing searches the segment.`,
	);

	/** @type {string[]} */
	const have = [];
	if (m1.state === 'done') have.push('format codecs');
	if (m4.state === 'done') have.push('text analysis');
	if (m5.state === 'done') have.push('a segment writer and reader');
	/** @type {string[]} */
	const pillarWrite = [];
	if (wal && walReady) pillarWrite.push('WAL');
	if (commit && groupState(ctx.leaves, GROUPS.commit.leaves) === 'done') pillarWrite.push('commit');
	if (merge && groupState(ctx.leaves, GROUPS.merge.leaves) === 'done') pillarWrite.push('segment merging');
	const pillarWriteSentence =
		pillarWrite.length > 0
			? `The write path is ${isState(m6)}: ${englishList(pillarWrite)}.`
			: `The write path is ${isState(m6)}.`;
	const scoring =
		m7.state === 'done' ? 'Scoring and query execution are done.' : 'Scoring and query execution do not.';
	const pillarEngine = seal(
		`${have.length > 0 ? `${englishList(have).charAt(0).toUpperCase()}${englishList(have).slice(1)} exist. ` : ''}${pillarWriteSentence} ${scoring}`,
	);

	const pillarCorrectness = seal(
		`${m1.state === 'done' && m2.state === 'done' && (m5.state === 'done' || m5.done > 0) ? 'Checksums, fuzzed parsers, the container commit, and segment read/write exist. ' : ''}${pillarWriteSentence} ${searchGap}`,
	);

	const pillarCloud = seal(
		`${m5.state === 'done' ? `Segment bytes can be written (${countState(m5)}). ` : ''}${pillarWriteSentence} ${startedPhrase(m12, 'Object storage')}. ${startedPhrase(m13, 'Query nodes', true)}.`,
	);

	const writePathLead =
		m6.state === 'done'
			? 'Local write path'
			: m6.state === 'in progress' || m6.done > 0
				? 'Local write path, partly in the tree'
				: 'Planned local write path';

	/** @type {Record<string, string>} */
	const tokens = {
		'%%ENGINE_SHORT%%': engineShort,
		'%%ENGINE_NOW%%': engineNow,
		'%%ENGINE_MODULES%%': engineModules,
		'%%LIBRARY_NOW%%': libraryNow,
		'%%WRITE_PATH_NOW%%': writePathNow,
		'%%DURABILITY_CELL%%': durabilityCell,
		'%%ENGINE_CALLS%%': engineCalls,
		'%%NO_PUBLIC_API%%': noPublicApi,
		'%%PRE_ALPHA%%': preAlpha,
		'%%M10_BLOCKED%%': m10Blocked,
		'%%M5_CELL%%': m5Cell,
		'%%PILLAR_ENGINE%%': pillarEngine,
		'%%PILLAR_CORRECTNESS%%': pillarCorrectness,
		'%%PILLAR_CLOUD%%': pillarCloud,
	};

	return {
		engineShort,
		engineNow,
		engineModules,
		libraryNow,
		writePathNow,
		durabilityCell,
		engineCalls,
		noPublicApi,
		preAlpha,
		m10Blocked,
		m5Cell,
		pillarEngine,
		pillarCorrectness,
		pillarCloud,
		walReady,
		writePathLead,
		tokens,
	};
}

/** @returns {StatusInput} */
function readInput() {
	const root = repoRoot();
	const read = (rel) => readFileSync(path.join(root, rel), 'utf8');
	const engineLib = read('crates/persia-engine/src/lib.rs');
	const dbRs = read('crates/persia-engine/src/db.rs');
	const snapshotRs = read('crates/persia-engine/src/db/snapshot.rs');
	const persiaLib = read('crates/persia/src/lib.rs');
	const blobLib = read('crates/persia-blob/src/lib.rs');
	const modules = publicModules(engineLib).filter((name) => name.length > 0);
	if (modules.length === 0) throw new Error('persia-engine: no public modules');
	const dbFns = exportedFns(dbRs);
	const pkg = workspacePackage(read('Cargo.toml'));
	const exports = (source) => /^pub (?:struct|enum|type|use|fn|mod|trait|const|static)\b/m.test(source);
	return {
		progress: loadProgress(),
		leaves: parseRoadmapLeaves(read('ROADMAP.md')),
		modules,
		dbFns,
		snapshotFns: exportedFns(snapshotRs),
		persiaExports: exports(persiaLib),
		blobExports: exports(blobLib),
		hasOpenOptions: /pub struct OpenOptions\b/.test(persiaLib) || /pub struct OpenOptions\b/.test(dbRs),
		hasPutStruct: dbFns.has('put_struct') || /pub fn put_struct\b/.test(persiaLib),
		hasPutBlob: dbFns.has('put_blob') || /pub fn put_blob\b/.test(persiaLib),
		hasGetBlob: dbFns.has('get_blob') || /pub fn get_blob\b/.test(persiaLib),
		hasQuery: modules.includes('query') || dbFns.has('search'),
		analyzers: builtinAnalyzerNames(read('crates/persia-analysis/src/registry.rs')),
		published: pkg.published,
		version: pkg.version,
	};
}

/** @returns {StatusText} */
export function loadStatus() {
	if (!cached) cached = buildStatus(readInput());
	return cached;
}

/**
 * Replace progress and status tokens. Safe in Markdown text and in JSON-LD,
 * because the sentences contain no double quotes.
 * @param {string} value
 */
export function substituteStatusText(value) {
	if (!value.includes('%%')) return value;
	let next = value;
	if (next.includes(PROGRESS_TOKEN)) next = next.replaceAll(PROGRESS_TOKEN, progressSentence());
	for (const [token, text] of Object.entries(loadStatus().tokens)) {
		if (next.includes(token)) next = next.replaceAll(token, text);
	}
	return next;
}

/** @returns {string[]} */
export function statusTokenNames() {
	return Object.keys(loadStatus().tokens);
}
