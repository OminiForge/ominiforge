/**
 * Extract the primary `path` argument from a tool call's JSON args string.
 *
 * Handles both the flat `{ path }` shape (read/write/edit single-file) and
 * the nested `{ sections: [{ path }] }` shape (edit multi-file).  Returns
 * `null` when args are missing, unparseable, or carry no path.
 */
export function extractArgsPath(args: string): string | null {
	if (!args || args === '{}') return null;
	try {
		const obj = JSON.parse(args) as Record<string, unknown>;
		if (typeof obj.path === 'string') return obj.path;
		// edit multi-file form: first section's path
		if (Array.isArray(obj.sections) && obj.sections.length > 0) {
			const first = obj.sections[0] as Record<string, unknown>;
			if (typeof first.path === 'string') return first.path;
		}
	} catch {
		/* partial or invalid JSON — common during streaming */
	}
	return null;
}
