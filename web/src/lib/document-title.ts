export const APP_NAME = 'Tagmail';

export function documentTitle(unreadCount: number): string {
	return unreadCount > 0 ? `(${unreadCount}) ${APP_NAME}` : APP_NAME;
}
