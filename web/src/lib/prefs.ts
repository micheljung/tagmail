export type Density = 'compact' | 'default' | 'comfortable';
export type InboxType = 'default' | 'unread';
export type ReadingPane = 'none' | 'right' | 'below';

export const PREF_KEYS = {
	density: 'tagmail.density',
	inboxType: 'tagmail.inboxType',
	readingPane: 'tagmail.readingPane',
	navWidth: 'tagmail.navWidth'
} as const;

export const DEFAULT_DENSITY: Density = 'default';
export const DEFAULT_INBOX_TYPE: InboxType = 'default';
export const DEFAULT_READING_PANE: ReadingPane = 'none';

export const NAV_WIDTH_MIN = 200;
export const NAV_WIDTH_MAX = 400;
export const NAV_WIDTH_DEFAULT = 236;

export function parseDensity(v: unknown): Density {
	return v === 'compact' || v === 'comfortable' ? v : DEFAULT_DENSITY;
}

export function parseInboxType(v: unknown): InboxType {
	return v === 'unread' ? v : DEFAULT_INBOX_TYPE;
}

export function parseReadingPane(v: unknown): ReadingPane {
	return v === 'right' || v === 'below' ? v : DEFAULT_READING_PANE;
}

/** Clamps to 200-400 and rounds; anything that is not a finite number gives the default. */
export function clampNavWidth(v: unknown): number {
	const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
	if (typeof n !== 'number' || !Number.isFinite(n)) return NAV_WIDTH_DEFAULT;
	return Math.min(NAV_WIDTH_MAX, Math.max(NAV_WIDTH_MIN, Math.round(n)));
}

/** Thread-row heights per density: desktop, coarse pointer, phone. */
const ROW_HEIGHTS: Record<Density, { desktop: number; coarse: number; phone: number }> = {
	default: { desktop: 40, coarse: 48, phone: 76 },
	comfortable: { desktop: 48, coarse: 56, phone: 88 },
	compact: { desktop: 32, coarse: 44, phone: 64 }
};

/** The list row height for a density; phone wins over a coarse pointer. */
export function rowHeightFor(density: Density, ctx: { coarse: boolean; phone: boolean }): number {
	const h = ROW_HEIGHTS[density];
	return ctx.phone ? h.phone : ctx.coarse ? h.coarse : h.desktop;
}
