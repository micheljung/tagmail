import { ShortcutMatcher, type ShortcutAction } from './keymap';

export type ShortcutHandler = (action: ShortcutAction, event: KeyboardEvent) => boolean;

/**
 * One document-level listener; screens push handlers, the newest that
 * returns `true` consumes the key. While a modal dialog is open only `?`
 * gets through, so Escape stays the dialog's own.
 */
class ShortcutRegistry {
	readonly #matcher = new ShortcutMatcher();
	readonly #handlers: ShortcutHandler[] = [];
	#attached = false;

	push(handler: ShortcutHandler): () => void {
		this.#attach();
		this.#handlers.push(handler);
		return () => {
			const at = this.#handlers.lastIndexOf(handler);
			if (at !== -1) this.#handlers.splice(at, 1);
		};
	}

	#attach(): void {
		if (this.#attached || typeof document === 'undefined') return;
		this.#attached = true;
		document.addEventListener('keydown', (event) => {
			if (event.defaultPrevented) return;
			const action = this.#matcher.match(event);
			if (!action) return;
			if (document.querySelector('dialog[open]') && action !== 'help') return;
			for (let i = this.#handlers.length - 1; i >= 0; i--) {
				if (this.#handlers[i](action, event)) {
					event.preventDefault();
					return;
				}
			}
		});
	}
}

export const shortcuts = new ShortcutRegistry();
