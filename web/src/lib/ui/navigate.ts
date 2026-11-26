import { goto } from '$app/navigation';
import { prefs } from '../stores';

/**
 * `goto` that keeps focus where it is while the list and the reading pane
 * share the screen; otherwise focus and scroll reset as usual.
 */
export function navigate(href: string, opts: Parameters<typeof goto>[1] = {}): Promise<void> {
	return goto(href, { ...opts, reset: !prefs.split });
}
