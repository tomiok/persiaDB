import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
	MILESTONE_TABLE_TOKEN,
	PROGRESS_TOKEN,
	loadProgress,
	milestoneTableMarkdown,
	parseProgress,
	progressSentence,
} from './progress.mjs';

const progressMd = readFileSync(
	path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../PROGRESS.md'),
	'utf8',
);

test('parseProgress matches the summary and the milestone rows', () => {
	const parsed = parseProgress(progressMd);
	assert.equal(parsed.done, loadProgress().done);
	assert.equal(parsed.total, loadProgress().total);
	assert.ok(parsed.milestones.length >= 16);
	assert.equal(parsed.milestones[0].id, 'M0');
	assert.equal(parsed.milestones.at(-1)?.id, 'M15');
	const sentence = progressSentence();
	assert.match(sentence, new RegExp(`${parsed.done} of ${parsed.total} roadmap leaves done \\(${parsed.percent}%\\)`));
	assert.match(sentence, new RegExp(parsed.snapshot));
	assert.equal(sentence.includes(PROGRESS_TOKEN), false);
	const table = milestoneTableMarkdown();
	assert.match(table, /\| \*\*M0\*\* /);
	assert.match(table, /\| \*\*M4\*\* /);
	assert.match(table, new RegExp(`In progress, ${parsed.milestones.find((ms) => ms.id === 'M4')?.done}/`));
	assert.equal(table.includes(MILESTONE_TABLE_TOKEN), false);
	assert.equal(table.includes('|'), true);
});

test('a truncated PROGRESS.md fails closed', () => {
	assert.throws(() => parseProgress('# no table\n'), /summary line/);
});

test('post-MVP milestone rows stay out of the MVP snapshot', () => {
	const text = `# Persia DB — Progress

**1/2 leaves done (50%)** · 0 in progress · \`█\` · last snapshot: 2026-10-26

## Milestones

| Milestone | State | Progress | Done | Doing | Leaves | Deps |
|---|---|---|--:|--:|--:|---|
| **M0** Bootstrap | ✅ done | \`█\` 100% | 1 | 0 | 1 | — |
| **M15** Release | ⏳ blocked | \`░\` 0% | 0 | 0 | 1 | M0 |

## Post-MVP

| Milestone | State | Progress | Done | Doing | Leaves | Deps |
|---|---|---|--:|--:|--:|---|
| **M16** Embedded SDKs (v1.1) | ⏳ blocked | \`░\` 0% | 0 | 0 | 18 | M15 |
`;
	const parsed = parseProgress(text);
	assert.deepEqual(
		parsed.milestones.map((ms) => ms.id),
		['M0', 'M15'],
	);
	assert.equal(parsed.done, 1);
	assert.equal(parsed.total, 2);
	assert.equal(parsed.inProgress, 0);
	assert.equal(
		parsed.milestones.some((ms) => ms.id === 'M16'),
		false,
	);
});
