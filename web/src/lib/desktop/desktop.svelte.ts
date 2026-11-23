import type {
	AddressDto,
	DesktopPairDto,
	DesktopStatusDto,
	MailtoDto,
	UpdateRequiredDto
} from '@tagmail/api-types';
import type { DraftInit } from '../compose/draft.svelte';
import type { Api } from '../transport/api';
import type { Listen, Unlisten } from '../transport/tauri';

export const PARTIAL_TEXT = 'Older results need a connection';
export const OFFLINE_TEXT = 'Offline. Showing the mail on this device; changes are sent later.';
export const DOWNLOAD_URL = 'https://github.com/micheljung/tagmail/releases';

function escapeHtml(text: string): string {
	return text.replace(
		/[&<>"']/g,
		(c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] ?? c
	);
}

/** A `mailto:` link as a new draft; the body is plain text. */
export function mailtoDraft(mailto: MailtoDto): DraftInit {
	const addresses = (list: string[]): AddressDto[] => list.map((email) => ({ name: null, email }));
	const body = mailto.body ?? '';
	return {
		to: addresses(mailto.to),
		cc: addresses(mailto.cc),
		bcc: addresses(mailto.bcc),
		subject: mailto.subject ?? '',
		html: body ? body.split(/\r?\n/).map(escapeHtml).join('<br>') : ''
	};
}

export interface DesktopHandlers {
	mailto(draft: DraftInit): void;
	pair(pairing: DesktopPairDto): void;
}

/** Sync state of the desktop app, fed by `/api/desktop/status` and its events. */
export class DesktopStore {
	status = $state<DesktopStatusDto | null>(null);
	/** The last "offline" error, shown verbatim until the server answers again. */
	offlineMessage = $state<string | null>(null);
	updateRequired = $state<UpdateRequiredDto | null>(null);
	/** A `tagmail://pair` link to prefill the pairing form with. */
	pairing = $state<DesktopPairDto | null>(null);

	offlineText = $derived(
		this.offlineMessage ??
			(this.status && this.status.paired && !this.status.online ? OFFLINE_TEXT : null)
	);

	progress = $derived.by(() => {
		const phase = this.status?.phase;
		if (phase?.kind !== 'bootstrapping') return null;
		return phase.total > 0 ? Math.floor((phase.done / phase.total) * 100) : 0;
	});

	setStatus(status: DesktopStatusDto): void {
		this.status = status;
		if (status.online) this.offlineMessage = null;
		if (status.phase.kind === 'updateRequired') {
			this.updateRequired = { server: status.phase.server, minClient: status.phase.minClient };
		}
	}

	async start(api: Api, listen: Listen, handlers: DesktopHandlers): Promise<Unlisten> {
		api.onOffline = (e) => (this.offlineMessage = e.message);
		const unlisteners = await Promise.all([
			listen<DesktopStatusDto>('desktop:status', (s) => this.setStatus(s)),
			listen<UpdateRequiredDto>('desktop:update-required', (u) => (this.updateRequired = u)),
			listen<MailtoDto>('desktop:mailto', (m) => handlers.mailto(mailtoDraft(m))),
			listen<DesktopPairDto>('desktop:pair', (p) => {
				this.pairing = p;
				handlers.pair(p);
			})
		]);
		try {
			this.setStatus(await api.desktopStatus());
		} catch {
			// Events keep the status current once the app reports it.
		}
		return () => {
			for (const unlisten of unlisteners) unlisten();
		};
	}
}

export const desktop = new DesktopStore();
