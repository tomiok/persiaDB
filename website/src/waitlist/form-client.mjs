import { prepareSubmission } from './submit.mjs';

const SENT = 'Sent. The note is encrypted; only the project owner can read it.';

/**
 * @param {ParentNode} [root]
 */
export function bindWaitlistForms(root = document) {
	for (const form of root.querySelectorAll('form[data-waitlist]')) {
		if (!(form instanceof HTMLFormElement)) continue;
		if (form.dataset.waitlistBound === '1') continue;
		form.dataset.waitlistBound = '1';
		form.addEventListener('submit', (event) => {
			event.preventDefault();
			void send(form);
		});
	}
}

/**
 * @param {HTMLFormElement} form
 */
async function send(form) {
	const status = form.querySelector('.waitlist-status');
	const button = form.querySelector('button[type="submit"]');
	const config = readConfig(form);
	if (!config || config.open !== true || typeof config.relay !== 'string' || typeof config.publicKey !== 'string') {
		write(status, 'The waitlist is not open yet.');
		return;
	}
	if (button instanceof HTMLButtonElement) button.disabled = true;
	try {
		const prepared = await prepareSubmission({
			publicKeyPem: config.publicKey,
			fields: {
				email: value(form, 'email'),
				name: value(form, 'name'),
				company: value(form, 'company'),
				useCase: value(form, 'use_case'),
				hp: value(form, 'hp'),
			},
		});
		if (prepared.status === 'spam') {
			write(status, SENT);
			form.reset();
			return;
		}
		const response = await fetch(config.relay, {
			method: 'POST',
			mode: 'cors',
			credentials: 'omit',
			referrerPolicy: 'no-referrer',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(prepared.body),
		});
		if (response.status === 429) {
			write(status, 'Too many attempts. Wait a minute and try again.');
			return;
		}
		if (!response.ok) {
			write(status, 'Could not send. Try again.');
			return;
		}
		write(status, SENT);
		form.reset();
	} catch (error) {
		const code = error && typeof error === 'object' && 'code' in error ? error.code : '';
		write(status, code === 'email' ? 'Check the email address.' : 'Could not send. Try again.');
	} finally {
		if (button instanceof HTMLButtonElement && config.open === true) button.disabled = false;
	}
}

/**
 * @param {HTMLFormElement} form
 */
function readConfig(form) {
	const raw = form.getAttribute('data-config');
	if (!raw) return null;
	try {
		return JSON.parse(decodeURIComponent(raw));
	} catch {
		return null;
	}
}

/**
 * @param {HTMLFormElement} form
 * @param {string} name
 */
function value(form, name) {
	const element = form.elements.namedItem(name);
	if (element && 'value' in element && typeof element.value === 'string') return element.value;
	return '';
}

/**
 * @param {Element | null} status
 * @param {string} text
 */
function write(status, text) {
	if (status) status.textContent = text;
}
