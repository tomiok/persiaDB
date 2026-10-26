// Substitutes PROGRESS.md into docs Markdown at build time.
// A paragraph that is only the table token becomes the milestone table.
// The summary token is replaced wherever it appears as text, including inside a sentence.

import { MILESTONE_TABLE_TOKEN, PROGRESS_TOKEN, milestoneTableMarkdown, progressSentence } from './progress.mjs';

/** @type {import('@astrojs/markdown-satteri').SatteriProcessorOptions['mdastPlugins']} */
export const progressMarkdownPlugin = {
	name: 'persia-progress',
	text(node, context) {
		if (!node.value.includes(PROGRESS_TOKEN)) return;
		context.setProperty(node, 'value', node.value.replaceAll(PROGRESS_TOKEN, progressSentence()));
	},
	paragraph(node, context) {
		if (context.textContent(node).trim() !== MILESTONE_TABLE_TOKEN) return;
		context.replaceNode(node, { raw: `${milestoneTableMarkdown()}\n`, mdxExpressions: false });
	},
	after(root, context) {
		const text = context.textContent(root);
		if (text.includes(PROGRESS_TOKEN) || text.includes(MILESTONE_TABLE_TOKEN)) {
			throw new Error('a progress token was left unreplaced in a docs page');
		}
	},
};
