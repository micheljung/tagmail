import { Api } from './api';
import { HttpTransport } from './http';

export * from './api';
export type {
	DeviceDto,
	LabelDto,
	MailboxDto,
	MessageDto,
	PairingCodeDto,
	PartDto,
	PasskeyDto,
	SessionDto,
	ThreadDetailDto,
	ThreadMessageDto
} from '@tagmail/api-types';
export * from './passkey';
export { HttpTransport } from './http';

/** The app's single API client; the desktop build swaps the transport here. */
export const api = new Api(new HttpTransport());
