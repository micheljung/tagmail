export type ShortcutAction =
	'next' | 'previous' | 'open' | 'back' | 'search' | 'help' | 'goInbox' | 'goLabel' | 'escape';

export interface ShortcutHelp {
	keys: string[];
	action: ShortcutAction;
	label: string;
}

/** Keys as typed, space-separated for sequences. */
const BINDINGS: Record<string, ShortcutAction> = {
	j: 'next',
	k: 'previous',
	o: 'open',
	Enter: 'open',
	u: 'back',
	'/': 'search',
	'?': 'help',
	'g i': 'goInbox',
	'g l': 'goLabel',
	Escape: 'escape'
};

export const HELP: readonly ShortcutHelp[] = [
	{ keys: ['j'], action: 'next', label: 'Older conversation' },
	{ keys: ['k'], action: 'previous', label: 'Newer conversation' },
	{ keys: ['o', 'Enter'], action: 'open', label: 'Open conversation' },
	{ keys: ['u'], action: 'back', label: 'Back to the list' },
	{ keys: ['/'], action: 'search', label: 'Search mail' },
	{ keys: ['g', 'i'], action: 'goInbox', label: 'Go to Inbox' },
	{ keys: ['g', 'l'], action: 'goLabel', label: 'Go to label' },
	{ keys: ['?'], action: 'help', label: 'Keyboard shortcuts' }
];

/** How long the first key of a sequence waits for the second. */
export const SEQUENCE_TIMEOUT_MS = 1500;

export interface KeyLike {
	key: string;
	ctrlKey: boolean;
	metaKey: boolean;
	altKey: boolean;
	target: EventTarget | null;
	isComposing?: boolean;
}

function isEditable(target: EventTarget | null): boolean {
	if (!target || typeof (target as Element).closest !== 'function') return false;
	const el = target as HTMLElement;
	if (el.isContentEditable) return true;
	const tag = el.tagName;
	if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
	if (tag !== 'INPUT') return false;
	const type = (el as HTMLInputElement).type;
	return !['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color'].includes(type);
}

function isActivatable(target: EventTarget | null): boolean {
	if (!target || typeof (target as Element).closest !== 'function') return false;
	return (target as Element).closest('button, a[href], summary, [role="button"]') !== null;
}

const PREFIXES = new Set(
	Object.keys(BINDINGS)
		.filter((k) => k.includes(' '))
		.map((k) => k.split(' ')[0])
);

/**
 * Turns key events into actions, Gmail style: single keys and `g`-prefixed
 * sequences, ignored while typing (Escape excepted) or with modifiers.
 */
export class ShortcutMatcher {
	#prefix: string | null = null;
	#prefixAt = 0;
	readonly #now: () => number;

	constructor(now: () => number = () => Date.now()) {
		this.#now = now;
	}

	get pending(): string | null {
		return this.#prefix;
	}

	match(event: KeyLike): ShortcutAction | null {
		if (event.isComposing || event.ctrlKey || event.metaKey || event.altKey) {
			this.#prefix = null;
			return null;
		}
		if (isEditable(event.target)) {
			return event.key === 'Escape' ? 'escape' : null;
		}
		const key = event.key;
		if (key === 'Enter' && isActivatable(event.target)) return null;
		if (this.#prefix !== null) {
			const prefix = this.#prefix;
			this.#prefix = null;
			if (this.#now() - this.#prefixAt <= SEQUENCE_TIMEOUT_MS) {
				const action = BINDINGS[`${prefix} ${key}`];
				if (action) return action;
			}
		}
		if (PREFIXES.has(key)) {
			this.#prefix = key;
			this.#prefixAt = this.#now();
			return null;
		}
		return BINDINGS[key] ?? null;
	}
}
