// Leaf counts for the docs site, read from the generated PROGRESS.md at build time.
// The website workflow also rebuilds when that file changes, so a newer snapshot
// reaches the published pages without a hand-edited number.

import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** Visible pages and the FAQ JSON-LD substitute this token. */
export const PROGRESS_TOKEN = '%%PROGRESS_SUMMARY%%';

/** A paragraph whose only text is this token becomes the milestone table. */
export const MILESTONE_TABLE_TOKEN = '%%MILESTONE_TABLE%%';

/**
 * Short "what it delivers" phrases. The counts, state, and dependencies come
 * from PROGRESS.md. A milestone missing here uses its PROGRESS.md name.
 * @type {Record<string, string>}
 */
const DELIVERS = {
	M0: 'Workspace, CI, tooling, docs scaffolding',
	M1: 'Encodings, codecs, header, frames',
	M2: 'Storage, memory, local file, container',
	M3: 'Collections, documents, manifests',
	M4: 'Tokenizers, stemmers, analyzers',
	M5: 'Segment writer and reader',
	M6: 'WAL, commit, merge, recovery',
	M7: 'Parser, BM25F, top-k, facets',
	M8: 'The zero-results cascade',
	M9: 'Pack file, remote blobs, GC',
	M10: 'Db and Collection, CLI, v0.1',
	M11: 'gRPC, HTTP, image',
	M12: 'S3, GCS, Azure, cache, replicate',
	M13: 'Lease, shards, router, Helm',
	M14: 'Rust and Go network clients',
	M15: 'Benchmarks, hardening, format freeze',
};

const STATE_LABEL = {
	done: 'Done',
	'in progress': 'In progress',
	ready: 'Ready',
	blocked: 'Blocked',
};

/**
 * @typedef {Object} Milestone
 * @property {string} id
 * @property {string} name
 * @property {'done' | 'in progress' | 'ready' | 'blocked'} state
 * @property {number} done
 * @property {number} doing
 * @property {number} leaves
 * @property {string} deps
 * @property {string} delivers
 */

/**
 * @typedef {Object} Progress
 * @property {number} done
 * @property {number} total
 * @property {number} percent
 * @property {number} inProgress
 * @property {string} snapshot
 * @property {Milestone[]} milestones
 */

/** @type {Progress | undefined} */
let cached;

/** @returns {string} */
function progressFile() {
	const fromHere = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../PROGRESS.md');
	if (existsSync(fromHere)) return fromHere;
	const fromCwd = path.resolve(process.cwd(), '../PROGRESS.md');
	if (existsSync(fromCwd)) return fromCwd;
	throw new Error(`PROGRESS.md not found (tried ${fromHere} and ${fromCwd})`);
}

/**
 * @param {string} text
 * @returns {Progress}
 */
export function parseProgress(text) {
	const summary = text.match(
		/\*\*(\d+)\/(\d+) leaves done \((\d+)%\)\*\* · (\d+) in progress · `[^`]*` · last snapshot: (\d{4}-\d{2}-\d{2})/,
	);
	if (!summary) {
		throw new Error('PROGRESS.md: could not parse the summary line');
	}
	const done = Number(summary[1]);
	const total = Number(summary[2]);
	const percent = Number(summary[3]);
	const inProgress = Number(summary[4]);
	const snapshot = summary[5];
	if (total === 0 || done > total || inProgress > total) {
		throw new Error(`PROGRESS.md: implausible summary ${done}/${total}, ${inProgress} in progress`);
	}

	/** @type {Milestone[]} */
	const milestones = [];
	const row =
		/^\| \*\*(M\d+)\*\* ([^|]+?) \| [^|]*\b(done|in progress|ready|blocked) \| [^|]*\| (\d+) \| (\d+) \| (\d+) \| ([^|]+?) \|$/gm;
	// The headline and the first table are M0–M15. A second table under Post-MVP uses the same
	// row shape; summing it would disagree with the MVP summary line.
	const mvpText = text.split(/^## Post-MVP$/m)[0];
	for (const match of mvpText.matchAll(row)) {
		const id = match[1];
		const state = /** @type {Milestone['state']} */ (match[3]);
		if (milestones.some((ms) => ms.id === id)) {
			throw new Error(`PROGRESS.md: duplicate milestone ${id}`);
		}
		const name = match[2].trim();
		const deps = match[7].trim();
		if (name.includes('|') || deps.includes('|')) {
			throw new Error(`PROGRESS.md: ${id} has a pipe in a table cell`);
		}
		milestones.push({
			id,
			name,
			state,
			done: Number(match[4]),
			doing: Number(match[5]),
			leaves: Number(match[6]),
			deps,
			delivers: DELIVERS[id] ?? name,
		});
	}
	if (milestones.length === 0) {
		throw new Error('PROGRESS.md: could not parse the milestone table');
	}

	const sumDone = milestones.reduce((sum, ms) => sum + ms.done, 0);
	const sumDoing = milestones.reduce((sum, ms) => sum + ms.doing, 0);
	const sumLeaves = milestones.reduce((sum, ms) => sum + ms.leaves, 0);
	if (sumDone !== done || sumDoing !== inProgress || sumLeaves !== total) {
		throw new Error(
			`PROGRESS.md: milestone rows sum to ${sumDone}/${sumLeaves} (${sumDoing} in progress), summary says ${done}/${total} (${inProgress} in progress)`,
		);
	}
	for (const ms of milestones) {
		if (ms.done + ms.doing > ms.leaves) {
			throw new Error(`PROGRESS.md: ${ms.id} counts exceed its leaf total`);
		}
	}

	return { done, total, percent, inProgress, snapshot, milestones };
}

/** @returns {Progress} */
export function loadProgress() {
	if (!cached) cached = parseProgress(readFileSync(progressFile(), 'utf8'));
	return cached;
}

/** Plain sentence, safe inside JSON-LD and Markdown. */
export function progressSentence() {
	const p = loadProgress();
	const marked = p.inProgress === 1 ? '1 marked in progress' : `${p.inProgress} marked in progress`;
	return `${p.done} of ${p.total} roadmap leaves done (${p.percent}%) in the ${p.snapshot} PROGRESS.md snapshot, with ${marked}`;
}

/** GitHub-flavored Markdown table. Counts come from PROGRESS.md. */
export function milestoneTableMarkdown() {
	const lines = [
		'| Milestone | Delivers | Depends on | Snapshot |',
		'| --- | --- | --- | --- |',
	];
	for (const ms of loadProgress().milestones) {
		const label = STATE_LABEL[ms.state];
		lines.push(
			`| **${ms.id}** ${ms.name} | ${ms.delivers} | ${ms.deps} | ${label}, ${ms.done}/${ms.leaves} |`,
		);
	}
	return lines.join('\n');
}
