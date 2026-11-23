export type ShortcutAction =
	| 'next'
	| 'previous'
	| 'open'
	| 'back'
	| 'search'
	| 'help'
	| 'goInbox'
	| 'goLabel'
	| 'escape'
	| 'select'
	| 'archive'
	| 'label'
	| 'move'
	| 'delete'
	| 'star'
	| 'markRead'
	| 'markUnread'
	| 'reply'
	| 'replyAll'
	| 'forward'
	| 'compose';

export interface ShortcutHelp {
	keys: string[];
	action: ShortcutAction;
	label: string;
	/** Help section. */
	group: 'Navigate' | 'Act' | 'Write';
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
	Escape: 'escape',
	x: 'select',
	e: 'archive',
	l: 'label',
	v: 'move',
	'#': 'delete',
	s: 'star',
	I: 'markRead',
	U: 'markUnread',
	r: 'reply',
	a: 'replyAll',
	f: 'forward',
	c: 'compose'
};

export const HELP: readonly ShortcutHelp[] = [
	{ keys: ['j'], action: 'next', label: 'Older conversation', group: 'Navigate' },
	{ keys: ['k'], action: 'previous', label: 'Newer conversation', group: 'Navigate' },
	{ keys: ['o', 'Enter'], action: 'open', label: 'Open conversation', group: 'Navigate' },
	{ keys: ['u'], action: 'back', label: 'Back to the list', group: 'Navigate' },
	{ keys: ['/'], action: 'search', label: 'Search mail', group: 'Navigate' },
	{ keys: ['g', 'i'], action: 'goInbox', label: 'Go to Inbox', group: 'Navigate' },
	{ keys: ['g', 'l'], action: 'goLabel', label: 'Go to label', group: 'Navigate' },
	{ keys: ['?'], action: 'help', label: 'Keyboard shortcuts', group: 'Navigate' },
	{ keys: ['x'], action: 'select', label: 'Select conversation', group: 'Act' },
	{ keys: ['e'], action: 'archive', label: 'Archive', group: 'Act' },
	{ keys: ['l'], action: 'label', label: 'Label as…', group: 'Act' },
	{ keys: ['v'], action: 'move', label: 'Move to…', group: 'Act' },
	{ keys: ['#'], action: 'delete', label: 'Delete', group: 'Act' },
	{ keys: ['s'], action: 'star', label: 'Star or unstar', group: 'Act' },
	{ keys: ['⇧', 'i'], action: 'markRead', label: 'Mark as read', group: 'Act' },
	{ keys: ['⇧', 'u'], action: 'markUnread', label: 'Mark as unread', group: 'Act' },
	{ keys: ['c'], action: 'compose', label: 'Compose', group: 'Write' },
	{ keys: ['r'], action: 'reply', label: 'Reply', group: 'Write' },
	{ keys: ['a'], action: 'replyAll', label: 'Reply all', group: 'Write' },
	{ keys: ['f'], action: 'forward', label: 'Forward', group: 'Write' }
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
