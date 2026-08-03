/** Global toast store: transient action feedback (connected, saved, failed),
 *  rendered by `Toasts.svelte` fixed at the viewport corner. Replaces the
 *  per-page inline `Notice` flash for action results, which forced every
 *  operation to surface in one shared banner and could not carry a failure
 *  tone. Toasts auto-dismiss; errors linger longer than successes. */

export type ToastTone = 'success' | 'error' | 'info';

export interface Toast {
	id: number;
	tone: ToastTone;
	message: string;
}

let nextId = 1;
const listeners = new Set<(toasts: Toast[]) => void>();
let toasts: Toast[] = [];

function emit() {
	for (const fn of listeners) fn(toasts);
}

/** Push a toast; it auto-dismisses after `ms` (default: 3s success/info,
 *  6s error). Returns the id so a caller can dismiss early. */
export function pushToast(message: string, tone: ToastTone = 'info', ms?: number): number {
	const id = nextId++;
	toasts = [...toasts, { id, tone, message }];
	emit();
	const ttl = ms ?? (tone === 'error' ? 6000 : 3000);
	setTimeout(() => dismissToast(id), ttl);
	return id;
}

export function dismissToast(id: number) {
	toasts = toasts.filter((t) => t.id !== id);
	emit();
}

/** Subscribe (Svelte store contract). */
export function subscribeToasts(fn: (toasts: Toast[]) => void): () => void {
	listeners.add(fn);
	fn(toasts);
	return () => listeners.delete(fn);
}
