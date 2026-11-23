import { Api } from './api';
import { HttpTransport } from './http';

export * from './api';
export * from './write';
export type {
	AccountDto,
	AccountPatchDto,
	AccountStatusDto,
	AccountTestDto,
	AddressDto,
	DeviceDto,
	LabelDto,
	LabelExportDto,
	MailboxDto,
	MessageDto,
	NewAccountDto,
	PairingCodeDto,
	PartDto,
	PasskeyDto,
	SessionDto,
	ThreadDetailDto,
	ThreadMessageDto,
	ThreadRowDto
} from '@tagmail/api-types';
export * from './passkey';
export { HttpTransport } from './http';

/** The app's single API client; the desktop build swaps the transport here. */
export const api = new Api(new HttpTransport());
