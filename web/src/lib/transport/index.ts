import type { ApiTransport } from '@tagmail/api-types';
import { Api } from './api';
import { HttpTransport } from './http';
import { TauriTransport, isDesktop } from './tauri';

export * from './api';
export * from './write';
export type {
	AccountDto,
	AccountPatchDto,
	AccountStatusDto,
	AccountTestDto,
	AddressDto,
	DesktopPairDto,
	DesktopPhaseDto,
	DesktopSettingsDto,
	DesktopStatusDto,
	DeviceDto,
	LabelDto,
	LabelExportDto,
	MailboxDto,
	MailtoDto,
	MessageDto,
	NewAccountDto,
	PairingCodeDto,
	PartDto,
	PasskeyDto,
	SessionDto,
	ThreadDetailDto,
	ThreadMessageDto,
	ThreadRowDto,
	UpdateRequiredDto
} from '@tagmail/api-types';
export * from './passkey';
export { HttpTransport } from './http';
export { TauriTransport, isDesktop } from './tauri';

/** One SPA build serves the web and the desktop app; the transport is picked at runtime. */
export function createTransport(): ApiTransport {
	return isDesktop() ? new TauriTransport() : new HttpTransport();
}

/** The app's single API client. */
export const api = new Api(createTransport());
