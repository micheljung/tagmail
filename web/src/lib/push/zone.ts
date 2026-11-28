import type { ReminderSettingsDto, ReminderSettingsPatchDto } from '@tagmail/api-types';

/** This device's IANA time zone, e.g. `Europe/Zurich`. */
export function deviceTimeZone(): string | undefined {
	try {
		return Intl.DateTimeFormat().resolvedOptions().timeZone || undefined;
	} catch {
		return undefined;
	}
}

/** Every IANA zone the browser knows, else just `current`. */
export function timeZones(current: string | null): string[] {
	const intl = Intl as { supportedValuesOf?: (key: 'timeZone') => string[] };
	const known = intl.supportedValuesOf?.('timeZone') ?? [];
	return current && !known.includes(current) ? [current, ...known] : known;
}

interface ZoneApi {
	updateReminderSettings(patch: ReminderSettingsPatchDto): Promise<ReminderSettingsDto>;
}

/**
 * Gives the server this device's zone for floating and all-day reminders,
 * unless the owner already has one.
 */
export async function claimTimeZone(api: ZoneApi, zone = deviceTimeZone()): Promise<void> {
	if (!zone) return;
	await api.updateReminderSettings({ timeZone: zone, ifUnset: true }).catch(() => undefined);
}
