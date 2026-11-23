import type { AddressDto } from '@tagmail/api-types';

const DAY_MS = 86_400_000;

export const UNKNOWN_SENDER = 'Unknown sender';

export function displayName(a: AddressDto | null): string {
	if (!a) return UNKNOWN_SENDER;
	const name = a.name?.trim();
	if (name) return name;
	const at = a.email.indexOf('@');
	return at > 0 ? a.email.slice(0, at) : a.email;
}

/** Short list date, Gmail style: time today, month and day this year, else numeric. */
export function listDate(unixSeconds: number, now: Date = new Date(), locale?: string): string {
	const d = new Date(unixSeconds * 1000);
	if (d.toDateString() === now.toDateString()) {
		return d.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' });
	}
	if (d.getFullYear() === now.getFullYear() && now.getTime() - d.getTime() < 365 * DAY_MS) {
		return d.toLocaleDateString(locale, { month: 'short', day: 'numeric' });
	}
	return d.toLocaleDateString(locale, { year: '2-digit', month: 'numeric', day: 'numeric' });
}

export function fullDate(unixSeconds: number, locale?: string): string {
	return new Date(unixSeconds * 1000).toLocaleString(locale, {
		weekday: 'short',
		year: 'numeric',
		month: 'short',
		day: 'numeric',
		hour: 'numeric',
		minute: '2-digit'
	});
}

/** "Ana, Ben, Cho" with first names when several people wrote. */
export function senderLine(senders: readonly AddressDto[]): string {
	if (senders.length === 0) return '';
	if (senders.length === 1) return displayName(senders[0]);
	return senders.map((s) => displayName(s).split(/\s+/)[0]).join(', ');
}

/** Stable hue (0–359) for a label key, so a label keeps its color everywhere. */
export function labelHue(key: string): number {
	let h = 2166136261;
	for (let i = 0; i < key.length; i++) {
		h ^= key.charCodeAt(i);
		h = Math.imul(h, 16777619);
	}
	return (h >>> 0) % 360;
}

export function addressList(list: readonly AddressDto[]): string {
	return list.map((a) => (a.name ? `${a.name} <${a.email}>` : a.email)).join(', ');
}

const SIZE_UNITS = ['B', 'KB', 'MB', 'GB'];

export function fileSize(bytes: number): string {
	let n = bytes;
	let unit = 0;
	while (n >= 1024 && unit < SIZE_UNITS.length - 1) {
		n /= 1024;
		unit++;
	}
	return `${unit === 0 ? n : n.toFixed(n < 10 ? 1 : 0)} ${SIZE_UNITS[unit]}`;
}

/** A label's swatch color: the one the player picked, else its stable hue. */
export function labelColor(key: string, color: string | null | undefined): string {
	return color ?? `oklch(0.66 0.14 ${labelHue(key)})`;
}
