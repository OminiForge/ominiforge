/**
 * Extract the primary `path` argument from a tool call's JSON args string.
 *
 * Handles both the flat `{ path }` shape (read/write single-file) and the
 * nested `{ edits: [{ path }] }` shape (edit's content-anchored batch form —
 * first entry's path). Returns `null` when args are missing, unparseable, or
 * carry no path.
 */
export function extractArgsPath(args: string): string | null {
	if (!args || args === '{}') return null;
	try {
		const obj = JSON.parse(args) as Record<string, unknown>;
		if (typeof obj.path === 'string') return obj.path;
		// edit's batch form: first entry's path
		if (Array.isArray(obj.edits) && obj.edits.length > 0) {
			const first = obj.edits[0] as Record<string, unknown>;
			if (typeof first.path === 'string') return first.path;
		}
	} catch {
		/* partial or invalid JSON — common during streaming */
	}
	return null;
}
