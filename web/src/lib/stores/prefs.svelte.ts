import {
	DEFAULT_DENSITY,
	DEFAULT_INBOX_TYPE,
	DEFAULT_READING_PANE,
	NAV_WIDTH_DEFAULT,
	PREF_KEYS,
	clampNavWidth,
	parseDensity,
	parseInboxType,
	parseReadingPane,
	type Density,
	type InboxType,
	type ReadingPane
} from '../prefs';

const PHONE_QUERY = '(max-width: 720px)';

function read(key: string): string | null {
	try {
		return localStorage.getItem(key);
	} catch {
		return null;
	}
}

function write(key: string, value: string, isDefault: boolean): void {
	try {
		if (isDefault) localStorage.removeItem(key);
		else localStorage.setItem(key, value);
	} catch {
		// Storage can be unavailable (private mode); the choice still applies now.
	}
}

function isPhone(): boolean {
	try {
		return matchMedia(PHONE_QUERY).matches;
	} catch {
		return false;
	}
}

function applyDensity(density: Density): void {
	if (typeof document !== 'undefined') document.documentElement.dataset.density = density;
}

class Prefs {
	density = $state<Density>(parseDensity(read(PREF_KEYS.density)));
	inboxType = $state<InboxType>(parseInboxType(read(PREF_KEYS.inboxType)));
	readingPane = $state<ReadingPane>(parseReadingPane(read(PREF_KEYS.readingPane)));
	navWidth = $state(clampNavWidth(read(PREF_KEYS.navWidth)));
	/** Kept current by the layout's media-query listener. */
	phone = $state(isPhone());
	/** Whether the list and the open thread sit side by side (or stacked). */
	split = $derived(this.readingPane !== 'none' && !this.phone);

	constructor() {
		applyDensity(this.density);
	}

	setDensity(v: Density): void {
		this.density = parseDensity(v);
		write(PREF_KEYS.density, this.density, this.density === DEFAULT_DENSITY);
		applyDensity(this.density);
	}

	setInboxType(v: InboxType): void {
		this.inboxType = parseInboxType(v);
		write(PREF_KEYS.inboxType, this.inboxType, this.inboxType === DEFAULT_INBOX_TYPE);
	}

	setReadingPane(v: ReadingPane): void {
		this.readingPane = parseReadingPane(v);
		write(PREF_KEYS.readingPane, this.readingPane, this.readingPane === DEFAULT_READING_PANE);
	}

	setNavWidth(v: number): void {
		this.navWidth = clampNavWidth(v);
		write(PREF_KEYS.navWidth, String(this.navWidth), this.navWidth === NAV_WIDTH_DEFAULT);
	}
}

export const prefs = new Prefs();
