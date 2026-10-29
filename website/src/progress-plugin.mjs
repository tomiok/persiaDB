// Substitutes PROGRESS.md into docs Markdown at build time.
// A paragraph that is only the table token becomes the milestone table.
// The summary token is replaced wherever it appears as text, including inside a sentence.

import { MILESTONE_TABLE_TOKEN, PROGRESS_TOKEN, milestoneTableMarkdown } from './progress.mjs';
import { statusTokenNames, substituteStatusText } from './status.mjs';

/** @type {import('@astrojs/markdown-satteri').SatteriProcessorOptions['mdastPlugins']} */
export const progressMarkdownPlugin = {
	name: 'persia-progress',
	text(node, context) {
		if (!node.value.includes('%%')) return;
		const next = substituteStatusText(node.value);
		if (next !== node.value) context.setProperty(node, 'value', next);
	},
	paragraph(node, context) {
		if (context.textContent(node).trim() !== MILESTONE_TABLE_TOKEN) return;
		context.replaceNode(node, { raw: `${milestoneTableMarkdown()}\n`, mdxExpressions: false });
	},
	after(root, context) {
		const text = context.textContent(root);
		const tokens = [PROGRESS_TOKEN, MILESTONE_TABLE_TOKEN, ...statusTokenNames()];
		const left = tokens.filter((token) => text.includes(token));
		if (left.length > 0) {
			throw new Error(`a status token was left unreplaced in a docs page: ${left.join(', ')}`);
		}
	},
};
