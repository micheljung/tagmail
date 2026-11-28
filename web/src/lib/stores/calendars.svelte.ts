import type { CalendarDto, CalendarPatchDto } from '@tagmail/api-types';
import type { Api } from '../transport';

export class CalendarsStore {
	calendars = $state<CalendarDto[]>([]);

	readonly #api: Api;

	constructor(api: Api) {
		this.#api = api;
	}

	of(accountId: number): CalendarDto[] {
		return this.calendars.filter((c) => c.accountId === accountId);
	}

	async load(): Promise<void> {
		this.calendars = await this.#api.calendars();
	}

	/** Shows a colour or visibility change at once; the server's answer then replaces it. */
	async patch(change: CalendarPatchDto): Promise<void> {
		const target = this.calendars.find(
			(c) => c.accountId === change.accountId && c.href === change.href
		);
		const before: Partial<Pick<CalendarDto, 'color' | 'visible'>> = {};
		if (target && change.color !== undefined) {
			before.color = target.color;
			target.color = change.color;
		}
		if (target && change.visible !== undefined) {
			before.visible = target.visible;
			target.visible = change.visible;
		}
		try {
			await this.#api.updateCalendar(change);
		} catch (e) {
			if (target) Object.assign(target, before);
			throw e;
		} finally {
			await this.load().catch(() => undefined);
		}
	}
}
