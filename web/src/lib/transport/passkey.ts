import type { Api } from './api';

interface CreationOptions {
	publicKey: PublicKeyCredentialCreationOptionsJSON;
}

interface RequestOptions {
	publicKey: PublicKeyCredentialRequestOptionsJSON;
}

/** Whether this browser can run a passkey ceremony from the server's JSON options. */
export function passkeysSupported(): boolean {
	const pkc = globalThis.PublicKeyCredential as Partial<typeof PublicKeyCredential> | undefined;
	return (
		typeof pkc?.parseCreationOptionsFromJSON === 'function' &&
		typeof pkc.parseRequestOptionsFromJSON === 'function'
	);
}

function toJSON(credential: Credential | null): unknown {
	if (!(credential instanceof PublicKeyCredential)) throw new Error('No passkey was chosen.');
	return credential.toJSON();
}

export async function signInWithPasskey(api: Api): Promise<void> {
	const challenge = await api.passkeyLoginStart();
	const publicKey = PublicKeyCredential.parseRequestOptionsFromJSON(
		(challenge.options as RequestOptions).publicKey
	);
	const credential = await navigator.credentials.get({ publicKey });
	await api.passkeyLoginFinish(challenge.ceremonyId, toJSON(credential));
}

export async function addPasskey(api: Api, name: string): Promise<void> {
	const challenge = await api.passkeyRegisterStart(name);
	const publicKey = PublicKeyCredential.parseCreationOptionsFromJSON(
		(challenge.options as CreationOptions).publicKey
	);
	const credential = await navigator.credentials.create({ publicKey });
	await api.passkeyRegisterFinish(challenge.ceremonyId, toJSON(credential));
}
