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

	async patch(change: CalendarPatchDto): Promise<void> {
		await this.#api.updateCalendar(change);
		await this.load();
	}
}
