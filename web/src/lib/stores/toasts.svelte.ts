export interface ToastAction {
	label: string;
	run: () => void;
}

export interface Toast {
	id: number;
	text: string;
	tone: 'info' | 'error' | 'progress';
	actions: ToastAction[];
}

export const TOAST_MS = 6000;
export const ERROR_TOAST_MS = 12_000;

/**
 * Snackbars, newest last. A `key` replaces the earlier toast with the same
 * key, so an op's "Archived" turns into its failure instead of stacking.
 */
export class Toasts {
	items = $state<Toast[]>([]);

	#next = 1;
	readonly #timers = new Map<number, ReturnType<typeof setTimeout>>();
	readonly #keys = new Map<string, number>();

	show(
		text: string,
		options: {
			tone?: Toast['tone'];
			actions?: ToastAction[];
			key?: string;
			/** 0 keeps it until dismissed. */
			duration?: number;
		} = {}
	): number {
		const tone = options.tone ?? 'info';
		const previous = options.key ? this.#keys.get(options.key) : undefined;
		const id = previous ?? this.#next++;
		const toast: Toast = { id, text, tone, actions: options.actions ?? [] };
		const at = this.items.findIndex((t) => t.id === id);
		if (at === -1) this.items = [...this.items, toast].slice(-3);
		else this.items = this.items.map((t) => (t.id === id ? toast : t));
		if (options.key) this.#keys.set(options.key, id);
		const timer = this.#timers.get(id);
		if (timer) clearTimeout(timer);
		const duration = options.duration ?? (tone === 'error' ? ERROR_TOAST_MS : TOAST_MS);
		if (duration > 0)
			this.#timers.set(
				id,
				setTimeout(() => this.dismiss(id), duration)
			);
		return id;
	}

	dismiss(id: number): void {
		const timer = this.#timers.get(id);
		if (timer) clearTimeout(timer);
		this.#timers.delete(id);
		for (const [key, value] of this.#keys) if (value === id) this.#keys.delete(key);
		this.items = this.items.filter((t) => t.id !== id);
	}

	dismissKey(key: string): void {
		const id = this.#keys.get(key);
		if (id !== undefined) this.dismiss(id);
	}
}
