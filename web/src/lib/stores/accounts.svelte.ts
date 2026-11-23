import type { AccountDto, AccountStatusDto } from '@tagmail/api-types';
import type { Api } from '../transport';

/** Degraded codes the app shows a banner for; the rest only show in account settings. */
export const BANNER_CODES = ['labels_degraded', 'no_sender_address'] as const;
export type BannerCode = (typeof BANNER_CODES)[number];

export interface AccountNotice {
	accountId: number;
	name: string;
	code: BannerCode;
}

/** Spreads hues apart for consecutive ids (golden angle). */
const HUE_STEP = 137.508;
const FULL_CIRCLE = 360;

/** A stable hue for the account marker. */
export function accountHue(id: number): number {
	return Math.round((id * HUE_STEP) % FULL_CIRCLE);
}

export function accountInitial(name: string): string {
	return (name.trim().charAt(0) || '?').toUpperCase();
}

/** Connected accounts and their sync status, for the nav, row markers and banners. */
export class AccountsStore {
	accounts = $state<AccountDto[]>([]);
	status = $state<AccountStatusDto[]>([]);
	loaded = $state(false);
	error = $state<string | null>(null);

	readonly #api: Api;
	#loading: Promise<void> | null = null;

	constructor(api: Api) {
		this.#api = api;
	}

	/** More than one account: views are unified and rows carry account markers. */
	multi = $derived(this.accounts.length > 1);

	notices = $derived(
		this.status.flatMap((s) =>
			BANNER_CODES.filter((code) => s.degraded.includes(code)).map((code) => ({
				accountId: s.id,
				name: s.name,
				code
			}))
		)
	);

	get(id: number): AccountDto | undefined {
		return this.accounts.find((a) => a.id === id);
	}

	name(id: number): string {
		return this.get(id)?.name ?? `Account ${id}`;
	}

	statusOf(id: number): AccountStatusDto | undefined {
		return this.status.find((s) => s.id === id);
	}

	load(): Promise<void> {
		this.#loading ??= this.#fetch().finally(() => (this.#loading = null));
		return this.#loading;
	}

	async #fetch(): Promise<void> {
		try {
			const [accounts, status] = await Promise.all([this.#api.accounts(), this.#api.status()]);
			this.accounts = accounts;
			this.status = status.accounts;
			this.error = null;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loaded = true;
		}
	}
}
