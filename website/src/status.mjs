// What the crates implement today.
//
// Milestone counts and done / in progress / ready / blocked come from PROGRESS.md.
// Which write-path pieces are finished comes from ROADMAP.md checkboxes.
// Module and function names come from the crate sources.
// Pages share the sentences below (%%TOKENS%%) instead of copying a crate list.
// A claim is emitted only when the leaves that define it are checked and the
// matching code is still in the tree. An unchecked follow-up (query execution,
// relaxation) stays unclaimed.

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
	queryModel: {
		leaves: ['7.1.1', '7.1.2.1', '7.1.2.2', '7.1.3', '7.1.4'],
		code: (ctx) => ctx.modules.includes('query'),
		done: 'a query AST, user-text compiler, filter DSL, and query limits',
		partial: 'part of the query model (not every leaf is done)',
	},
	stats: {
		leaves: ['7.2.1', '7.2.2'],
		code: (ctx) => ctx.modules.includes('stats'),
		done: 'collection statistics (document count, document frequency, average field length)',
		partial: 'collection statistics (not every leaf is done)',
	},
	bm25: {
		leaves: ['7.3.1', '7.3.2', '7.3.3', '7.3.4'],
		code: (ctx) => ctx.modules.includes('score'),
		done: 'BM25F scoring with static rank and score bounds',
		partial: 'BM25F scoring (not every leaf is done)',
	},
	matching: {
		leaves: ['7.4.1', '7.4.2', '7.4.3', '7.4.4.1', '7.4.4.2', '7.4.4.3'],
		code: (ctx) => ctx.modules.includes('search'),
		done: 'matching and score top-k over one view',
		partial: 'part of matching (not every leaf is done)',
	},
	request: {
		leaves: ['7.5.1', '7.5.2', '7.5.3', '7.5.4'],
		code: (ctx) => ctx.hasCollectionSearch,
		done: 'a collection search with sorts, pages, facets, and stored fields (CollectionViews::search)',
		partial: 'part of the collection search (not every leaf is done)',
	},
};

/** Section 6.6. Not part of the write-path claim until every leaf is checked. */
const OPEN_LEAVES = ['6.6.1', '6.6.2', '6.6.3', '6.6.4', '6.6.5'];

/** Orchestrator, rank order, budget, and response metadata. Not the L1–L5 transforms. */
const RELAX_FRAMEWORK = ['8.1.1', '8.1.2', '8.1.3', '8.1.4', '8.1.5'];

/** One leaf from each cascade level after exact. Unchecked means that level is not claimed. */
const RELAX_LEVELS = ['8.2.1', '8.2.2.1', '8.2.3', '8.2.4.1', '8.2.5'];

/**
 * L2 drop_terms builds a query only once importance, the ladder, and the single-term skip are checked.
 * 8.2.2.1 alone is the ordering, not the step.
 */
const RELAX_DROP = ['8.2.2.1', '8.2.2.2', '8.2.2.3'];

/** Representative leaf for each level that is still a pending transform beside drop_terms. */
const RELAX_STILL = [
	['8.2.1', 'synonyms'],
	['8.2.3', 'prefix'],
	['8.2.4.1', 'fuzzy'],
	['8.2.5', 'the fallback'],
];

const FILTER_LEAVES = ['7.4.5'];
const DEADLINE_LEAVES = ['7.6.2', '7.6.3'];
const EXPLAIN_LEAVES = ['7.7.1', '7.7.2', '7.7.3'];

/** Pack file. Hashing and chunking can exist before this leaf. */
const PACK_LEAVES = ['9.2.1'];

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
 * @property {boolean} hasSearch A search on the database handle (persia::Db or Database), the SPEC call.
 * @property {boolean} hasCollectionSearch The engine's CollectionViews::search, over a collection snapshot.
 * @property {boolean} hasRelaxSearch CollectionViews::search_relaxed, the cascade orchestrator.
 * @property {boolean} hasDropTerms L2 wired to its transform (not the pending stub).
 * @property {boolean} hasSynonyms L1 wired to its transform (not the pending stub).
 * @property {boolean} hasPrefix L3 wired to its transform (not the pending stub).
 * @property {boolean} hasDeadline Execution::deadline, a search deadline.
 * @property {boolean} hasExplain SearchRequest.explain and the Explanation tree.
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
 * @property {string} queryPathNow
 * @property {string} relaxationNow
 * @property {string} blobNow
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
 * Whether an `impl <type>` block of `source` declares `pub fn <method>` (generic or not). Each impl body is cut at
 * its matching brace, so a method of another impl later in the file does not count. Braces inside comments and
 * strings are counted too; Rust sources keep those balanced in practice, and a miss fails loudly in groupPhrase.
 * @param {string} source
 * @param {string} type
 * @param {string} method
 * @returns {boolean}
 */
export function implHasMethod(source, type, method) {
	const head = new RegExp(`^impl(?:<[^>]*>)?\\s+${type}\\b[^{]*\\{`, 'gm');
	const fn = new RegExp(`^\\s*pub fn ${method}\\s*[<(]`, 'm');
	for (const match of source.matchAll(head)) {
		let depth = 1;
		let i = (match.index ?? 0) + match[0].length;
		const start = i;
		for (; i < source.length && depth > 0; i++) {
			if (source[i] === '{') depth++;
			else if (source[i] === '}') depth--;
		}
		if (fn.test(source.slice(start, i))) return true;
	}
	return false;
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
 * @param {string} text
 */
function capitalize(text) {
	return `${text.charAt(0).toUpperCase()}${text.slice(1)}`;
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
 * @param {string[]} ids
 * @param {boolean} code
 * @param {string} label
 * @returns {'done' | 'partial' | 'todo'}
 */
function requireDone(ctx, ids, code, label) {
	const state = groupState(ctx.leaves, ids);
	if (state === 'done' && !code) {
		throw new Error(`ROADMAP.md marks ${label} done but the matching code is not in the tree`);
	}
	return state;
}

/**
 * @param {string[]} items
 * @returns {string}
 */
function areNotBuilt(items) {
	const verb = items.length === 1 ? 'is not built' : 'are not built';
	return `${englishList(items)} ${verb}`;
}

/**
 * Plan and orchestrator versus the levels that loosen a query.
 * L0 always runs. A later level is claimed only when its representative leaf is checked and its transform is wired:
 * L1 synonyms, L2 drop_terms (its three leaves), L3 prefix. Fuzzy and the fallback stay unclaimed until theirs are.
 * @param {StatusInput} ctx
 * @param {Milestone} m8
 */
function relaxationFacts(ctx, m8) {
	const count = `${m8.done}/${m8.leaves}`;
	const framework = requireDone(ctx, RELAX_FRAMEWORK, ctx.hasRelaxSearch, 'relaxation');
	const drop = requireDone(ctx, RELAX_DROP, ctx.hasDropTerms, 'drop_terms');
	const synonyms = requireDone(ctx, ['8.2.1'], ctx.hasSynonyms, 'synonyms');
	const prefix = requireDone(ctx, ['8.2.3'], ctx.hasPrefix, 'prefix');
	const synonymsBuild = synonyms === 'done' && ctx.hasSynonyms;
	const prefixBuild = prefix === 'done' && ctx.hasPrefix;
	const dropBuild = drop === 'done' && ctx.hasDropTerms;
	/** @type {string[]} */
	const built = ['L0 exact'];
	if (synonymsBuild) built.push('L1 synonyms');
	if (dropBuild) built.push('L2 drop_terms');
	if (prefixBuild) built.push('L3 prefix');
	const builtPhrase = `${englishList(built)} build queries`;
	const levels = groupState(ctx.leaves, RELAX_LEVELS);
	const exactOnly = framework === 'done' && levels === 'todo' && drop === 'todo' && ctx.hasRelaxSearch;
	const levelsDone = m8.state === 'done' && levels === 'done' && ctx.hasRelaxSearch;
	const partial = framework === 'done' && ctx.hasRelaxSearch && levels !== 'done' && built.length > 1;
	const unstarted = m8.done === 0 && m8.state !== 'in progress' && framework === 'todo' && drop === 'todo' && levels === 'todo';
	/** @type {string[]} */
	const missing = RELAX_STILL.filter(([id]) => groupState(ctx.leaves, [id]) !== 'done').map(([, name]) => name);
	const typoOpen = missing.includes('fuzzy');
	/** @type {string} */
	let cell;
	/** @type {string} */
	let now;
	/** @type {string} */
	let gap;
	if (levelsDone) {
		cell = `Done (${m8.id}, ${count}).`;
		now = `Relaxation is done (${m8.id}, ${count}).`;
		gap = '';
	} else if (framework === 'done' && levels === 'done' && ctx.hasRelaxSearch) {
		cell = `In progress (${m8.id}, ${count}). Every cascade level builds a query. Not every M8 leaf is done.`;
		now = `Relaxation is in progress (${m8.id}, ${count}). Every cascade level builds a query. Not every M8 leaf is done.`;
		gap = `Relaxation is in progress (${m8.id}, ${count}): every cascade level builds a query.`;
	} else if (partial && missing.length > 0) {
		const still = areNotBuilt(missing);
		const typo = typoOpen ? ' A typo is not corrected.' : '';
		// L3 can match a one-term query whose analyzed form is a prefix. That sentence is only true while prefix does not build.
		const oneTerm = typoOpen && !prefixBuild && missing.includes('the fallback') ? ' A one-term typo still returns no hits.' : '';
		const corrected = groupState(ctx.leaves, ['8.2.4.3']) !== 'done' ? ' Corrected terms stay empty.' : '';
		/** @type {string[]} */
		const details = [];
		if (synonymsBuild) {
			details.push('L1 expands synonym runs as alternatives, and QueryLimits can cut or skip that step');
		}
		if (dropBuild) {
			details.push(
				'L2 drops terms least important first (a stopword, then a lower IDF, then a later position); the ladder is 75%, 50%, then one term, and a query of one term skips L2',
			);
		}
		if (prefixBuild) {
			details.push(
				'L3 prefix-expands the last analyzed term and other analyzed terms of at least 3 characters, at most 50 terms kept by collection df, and QueryLimits can cut or skip that step',
			);
		}
		const detail = details.length > 0 ? `${details.join('. ')}. ` : '';
		const fewer = dropBuild ? ' A query of two or more terms can match fewer of them.' : '';
		cell = `In progress (${m8.id}, ${count}). ${capitalize(builtPhrase)}. ${capitalize(still)}.${typo}`;
		now = `Relaxation is in progress (${m8.id}, ${count}). RelaxPlan, CollectionViews::search_relaxed, step-first ranking, the time budget, and the response metadata are library code. ${capitalize(builtPhrase)}. ${detail}${capitalize(still)}.${corrected}${typo}${fewer}${oneTerm}`;
		gap = `Relaxation is in progress (${m8.id}, ${count}): ${builtPhrase}, and ${still}.`;
	} else if (exactOnly) {
		cell = `In progress (${m8.id}, ${count}). The orchestrator runs, and only the exact level builds a query, so a typo still returns no hits.`;
		now = `Relaxation is in progress (${m8.id}, ${count}). RelaxPlan, CollectionViews::search_relaxed, step-first ranking, the time budget, and the response metadata are library code. Levels after exact do not build a query, so a search that matches nothing still returns no hits. Synonyms, dropped terms, prefix and fuzzy as cascade steps, the fallback, and corrected terms are not written.`;
		gap = `Relaxation is in progress (${m8.id}, ${count}): only the exact level builds a query.`;
	} else if (unstarted) {
		cell = `Not started (${m8.id}). On by default in the design.`;
		now = `Relaxation has not been started (${m8.id}). On by default in the design.`;
		gap = `Relaxation has not been started (${m8.id}).`;
	} else {
		cell = `In progress (${m8.id}, ${count}). On by default in the design. Not every cascade level builds a query.`;
		now = `Relaxation is in progress (${m8.id}, ${count}). Not every cascade level builds a query.`;
		gap = `Relaxation is in progress (${m8.id}, ${count}).`;
	}
	const later = englishList(built.slice(1));
	const lead = levelsDone
		? ''
		: partial && missing.length > 0
			? `Relaxation is in progress (${m8.id}, ${count}): ${later} can loosen a query${typoOpen ? ', and a typo is not corrected' : ''}.`
			: exactOnly
				? `Relaxation is in progress (${m8.id}, ${count}): only the exact level builds a query, so a search that matches nothing returns no hits.`
				: unstarted
					? `Relaxation has not been started (${m8.id}).`
					: `Relaxation is ${isState(m8)}. Not every cascade level builds a query.`;
	return {
		cell: seal(cell),
		now: seal(now),
		gap: gap ? seal(gap) : '',
		lead: lead ? seal(lead) : '',
	};
}

/**
 * @param {StatusInput} ctx
 * @param {Milestone} m9
 */
function blobFacts(ctx, m9) {
	const count = `${m9.done}/${m9.leaves}`;
	if (!ctx.blobExports) {
		const bare = `persia-blob exports no API (${countState(m9)}).`;
		return { cell: seal(bare), now: seal(bare) };
	}
	if (m9.state === 'done') {
		const bare = `The blob store is done (${m9.id}, ${count}).`;
		return { cell: seal(bare), now: seal(bare) };
	}
	const pack = groupState(ctx.leaves, PACK_LEAVES);
	if (pack === 'todo') {
		return {
			cell: seal(`In progress (${m9.id}, ${count}): hashing, chunking, and manifests. The pack file is not written.`),
			now: seal(
				`The blob store is in progress (${m9.id}, ${count}). persia-blob exports content hashing, chunking, and blob manifests. The pack file, remote blobs, document integration, and garbage collection are not written.`,
			),
		};
	}
	const now = `The blob store is in progress (${m9.id}, ${count}). persia-blob exports an API. Not every blob leaf is done.`;
	return { cell: seal(`In progress (${m9.id}, ${count}).`), now: seal(now) };
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
	const relax = relaxationFacts(ctx, m8);
	const blob = blobFacts(ctx, m9);

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
	const queryModel = groupPhrase(ctx, 'queryModel');
	const stats = groupPhrase(ctx, 'stats');
	const bm25 = groupPhrase(ctx, 'bm25');
	const matching = groupPhrase(ctx, 'matching');
	const request = groupPhrase(ctx, 'request');
	const queryPieces = [queryModel, stats, bm25, matching, request].filter((item) => item !== null);
	// A ranked search exists as library code once the request leaves are done and the call is in the tree. It is
	// not the SPEC's search on the database handle (hasSearch), which is the embedded API (M10).
	const collectionSearch =
		ctx.hasCollectionSearch && groupState(ctx.leaves, GROUPS.request.leaves) === 'done';
	const searches = m7.state === 'done' || ctx.hasSearch || collectionSearch;
	const searchGap = ctx.hasSearch || m7.state === 'done'
		? ''
		: collectionSearch
			? 'A collection snapshot can be searched as library code. There is no search call on the database handle.'
			: request && ctx.hasCollectionSearch
				? 'A collection search call is in the tree, but not every leaf of it is done.'
				: 'Nothing searches.';
	let querySentence = '';
	if (m7.state !== 'done') {
		if (queryPieces.length > 0) {
			querySentence = `Its query path is ${isState(m7)}: ${englishList(queryPieces)}.`;
		} else if (ctx.hasQuery) {
			querySentence = `A query module is in the tree. The query milestone is ${isState(m7)}.`;
		} else if (m7.done === 0 && m7.state !== 'in progress') {
			querySentence = `There is no query engine (${countState(m7)}).`;
		} else {
			querySentence = `The query engine is ${isState(m7)}.`;
		}
	}
	if (relax.gap) gaps.push(relax.gap);
	if (searchGap) gaps.push(searchGap);

	const engineShort = seal(
		`persia-engine implements ${englishList(engineBits)}. ${writeSentence} ${querySentence} ${gaps.join(' ')}`,
	);

	const engineModules = seal(englishList(ctx.modules));
	const engineNow = seal(`${engineShort} Public modules: ${engineModules}.`);

	const cloud =
		m12.state === 'done'
			? 'S3, GCS, and Azure are implemented.'
			: `${startedPhrase(m12, 'S3, GCS, and Azure', true)}.`;
	const blobSentence = blob.now;
	const persia = ctx.persiaExports
		? 'The persia crate exports a public API.'
		: 'The public persia crate exports no Db and no Collection.';
	const unpublished = ctx.published ? '' : ` The crates are unpublished (publish = false, version ${ctx.version}).`;
	const libraryNow = seal(
		`persia-format implements the on-disk codecs (${countState(m1)}). persia-storage implements the Storage trait, memory and local-file backends, and the single-file container (${countState(m2)}). ${cloud} ${engineShort} persia-analysis implements tokenizers and the built-in analyzers ${englishList(ctx.analyzers)} (${countState(m4)}). It does not search. ${blobSentence} ${persia}${unpublished}`,
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
			? 'Open and recovery orchestration is done (single-writer enforcement, persia verify, and the crash-loop harness).'
			: 'Open and recovery orchestration (single-writer enforcement, persia verify, and the crash-loop harness) is not done.';
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
	const getSentence =
		(ctx.snapshotFns.has('get') ? ' CollectionSnapshot::get fetches one document by id.' : '') +
		(collectionSearch ? ' CollectionViews::search runs a ranked search over a collection snapshot.' : '');
	/** @type {string[]} */
	const absent = [];
	if (!ctx.persiaExports) absent.push('persia::Db');
	if (!ctx.hasSearch) absent.push(collectionSearch ? 'search on the database handle' : 'search');
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
			: `The public persia crate exports no Db and no Collection.${m7.state === 'done' || ctx.hasSearch ? '' : collectionSearch ? ' A search runs only through persia-engine (CollectionViews::search).' : ' Nothing runs a search.'}${unpublished}`,
	);

	const canWrite = ctx.dbFns.has('put') && ctx.dbFns.has('delete') && ctx.dbFns.has('commit');
	const queryClause =
		m7.state === 'done' || ctx.hasSearch
			? 'A search call is in the tree.'
			: collectionSearch
				? 'persia-engine runs a ranked search over a collection snapshot: BM25F, filters, sorts, facets, and pages.'
				: bm25 && queryModel
				? `A query compiler and a BM25F scorer are in the tree.${matching ? ' Score top-k over one view is library code.' : ''} Nothing runs a search.`
				: ctx.hasQuery || bm25
					? 'Query code is in the tree. Nothing runs a search.'
					: 'Nothing runs a search.';
	const preAlpha = seal(
		`Persia DB is pre-alpha. ${ctx.persiaExports ? 'The persia crate exports a public API.' : 'The public persia crate has no Db::open.'} ${canWrite ? 'persia-engine can put, delete, and commit a document.' : 'The engine does not yet put documents.'} ${queryClause}`,
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

	const segmentSearch = collectionSearch
		? 'Segments are searched through CollectionViews::search.'
		: matching
			? 'A segment view can be matched. There is no search call.'
			: 'Nothing searches the segment.';
	const m5Cell = seal(
		m5.state === 'done'
			? `Done (${m5.id}, ${m5.done}/${m5.leaves}). ${segmentSearch}`
			: `${countState(m5)}. ${segmentSearch}`,
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
		m7.state === 'done'
			? 'Scoring and query execution are done.'
			: bm25 && collectionSearch
				? 'BM25F scoring and a collection search are in the tree (CollectionViews::search). There is no search call on the database handle.'
				: bm25 && matching
				? 'BM25F scoring is in the tree. Score top-k over one view returns hits. There is no search call.'
				: bm25
					? 'BM25F scoring is in the tree. Query execution does not return hits.'
					: 'Scoring and query execution do not return hits.';
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

	const deadlineState = requireDone(ctx, DEADLINE_LEAVES, ctx.hasDeadline, 'deadlines');
	const explainState = requireDone(ctx, EXPLAIN_LEAVES, ctx.hasExplain, 'explain');
	const filtersDone = groupState(ctx.leaves, FILTER_LEAVES) === 'done' && ctx.modules.includes('search');
	/** @type {string[]} */
	const requestBits = [];
	if (filtersDone) requestBits.push('filters');
	requestBits.push('sorts', 'cursor pages', 'facets');
	if (deadlineState === 'done') requestBits.push('deadlines');
	if (explainState === 'done') requestBits.push('explain');
	const featuresLead = seal(
		`${
			searches
				? collectionSearch && !ctx.hasSearch
					? 'Ranked full-text search, filters, sorts, and facets run as library code in persia-engine, over a collection snapshot.'
					: 'A search call is in the tree.'
				: bm25 && queryModel
					? 'The query compiler, the filter DSL, and BM25F scoring exist as library code. Nothing runs a search.'
					: 'Nothing runs a search.'
		} ${ctx.persiaExports ? '' : 'There is no Db to open yet (M10).'} ${relax.lead} The write path is ${isState(m6)}.`,
	);
	const queryCell = seal(
		collectionSearch
			? `Library code (${countState(m7)}): the compiler, the filter DSL, and the scorer return ranked hits through CollectionViews::search.`
			: bm25 && queryModel
				? `Compiler, filter DSL, and scorer are library code (${countState(m7)}). They do not return hits.`
				: `${capitalize(isState(m7))}.`,
	);
	const matchingCell = seal(
		collectionSearch
			? `Library code (${countState(m7)}): ${englishList(requestBits)}, through CollectionViews::search.`
			: matching
				? `Matching over one view is library code (${countState(m7)}). Sorts, cursor pages, and facets are not done.`
				: `${capitalize(isState(m7))}.`,
	);
	const relaxationCell = relax.cell;
	const queryPathNow = seal(
		`Text analysis is ${isState(m4)}. The query milestone is ${isState(m7)}. ${matchingCell} ${relax.gap}`,
	);
	const openCell = seal(
		ctx.persiaExports
			? 'The persia crate exports a public API.'
			: `${capitalize(isState(m10))}. ${collectionSearch && ctx.dbFns.has('open') ? 'Below it, persia_engine::db::Database opens a file and CollectionViews::search searches it.' : ''}`,
	);

	/** @type {Record<string, string>} */
	const milestoneTokens = {};
	for (const ms of ctx.progress.milestones) {
		milestoneTokens[`%%STATE_${ms.id}%%`] = seal(`${capitalize(isState(ms))}.`);
	}

	/** @type {Record<string, string>} */
	const tokens = {
		...milestoneTokens,
		'%%FEATURES_LEAD%%': featuresLead,
		'%%QUERY_CELL%%': queryCell,
		'%%MATCHING_CELL%%': matchingCell,
		'%%RELAXATION_CELL%%': relaxationCell,
		'%%RELAXATION_NOW%%': relax.now,
		'%%BLOB_CELL%%': blob.cell,
		'%%BLOB_NOW%%': blob.now,
		'%%QUERY_PATH_NOW%%': queryPathNow,
		'%%OPEN_CELL%%': openCell,
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
		queryPathNow,
		relaxationNow: relax.now,
		blobNow: blob.now,
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
	const relaxedRs = read('crates/persia-engine/src/db/relaxed.rs');
	const relaxRs = read('crates/persia-engine/src/relax.rs');
	const execRs = read('crates/persia-engine/src/db/exec.rs');
	const searchRs = read('crates/persia-engine/src/db/search.rs');
	const explainRs = read('crates/persia-engine/src/search/explain.rs');
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
		hasQuery: modules.includes('query'),
		hasSearch: dbFns.has('search') || /pub fn search\b/.test(persiaLib),
		hasCollectionSearch: ['crates/persia-engine/src/db/search.rs', 'crates/persia-engine/src/db/views.rs'].some((rel) =>
			implHasMethod(read(rel), 'CollectionViews', 'search'),
		),
		hasRelaxSearch: implHasMethod(relaxedRs, 'CollectionViews', 'search_relaxed'),
		hasDropTerms: /RelaxLevel::DropTerms => Arc::new\(drop_terms::DropTerms\)/.test(relaxRs),
		hasSynonyms: /RelaxLevel::Synonyms => Arc::new\(synonyms::Synonyms\)/.test(relaxRs),
		hasPrefix: /RelaxLevel::Prefix => Arc::new\(prefix::Prefix\)/.test(relaxRs),
		hasDeadline: implHasMethod(execRs, 'Execution', 'deadline'),
		hasExplain: /pub explain: bool/.test(searchRs) && /pub enum Explanation\b/.test(explainRs),
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
