/**
 * The push settings this device last applied, kept where the service worker
 * can read them, so a re-registration or an endpoint renewal sends them
 * instead of letting the server fall back to its defaults.
 */
export interface PushPrefs {
	endpoint: string;
	preview: boolean;
	/** `null` = all accounts. */
	accountIds: number[] | null;
}

export interface PushPrefStore {
	get(): Promise<PushPrefs | undefined>;
	set(prefs: PushPrefs): Promise<void>;
	clear(): Promise<void>;
}

const DB_NAME = 'tagmail-push';
const DB_VERSION = 1;
const STORE = 'prefs';
const KEY = 'device';

function settle<T>(request: IDBRequest<T>): Promise<T> {
	return new Promise((resolve, reject) => {
		request.onsuccess = () => resolve(request.result);
		request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
	});
}

function open(factory: IDBFactory): Promise<IDBDatabase> {
	const request = factory.open(DB_NAME, DB_VERSION);
	request.onupgradeneeded = () => request.result.createObjectStore(STORE);
	return settle(request);
}

export function idbPushPrefs(factory: () => IDBFactory = () => indexedDB): PushPrefStore {
	const run = async <T>(
		mode: IDBTransactionMode,
		op: (store: IDBObjectStore) => IDBRequest<T>
	): Promise<T> => {
		const db = await open(factory());
		try {
			const tx = db.transaction(STORE, mode);
			const done = new Promise<void>((resolve, reject) => {
				tx.oncomplete = () => resolve();
				tx.onerror = tx.onabort = () =>
					reject(tx.error ?? new Error('IndexedDB transaction aborted'));
			});
			const [result] = await Promise.all([settle(op(tx.objectStore(STORE))), done]);
			return result;
		} finally {
			db.close();
		}
	};
	return {
		get: () => run('readonly', (s) => s.get(KEY) as IDBRequest<PushPrefs | undefined>),
		set: async (prefs) => void (await run('readwrite', (s) => s.put(prefs, KEY))),
		clear: async () => void (await run('readwrite', (s) => s.delete(KEY)))
	};
}

export const pushPrefs = idbPushPrefs();
