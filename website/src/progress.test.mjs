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
