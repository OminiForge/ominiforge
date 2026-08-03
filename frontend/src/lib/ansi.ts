/** Minimal ANSI SGR (Select Graphic Rendition) parser: converts a string
 *  containing ANSI escape sequences into an array of styled segments.
 *  Only the 8/16-color palette, bold, dim, italic, underline, and reset are
 *  supported — the subset that realistically appears in shell tool output
 *  (compiler colors, direnv banners, git status, etc.). Non-SGR sequences
 *  (cursor movement, erase, OSC hyperlinks) are silently stripped. */

/** A run of text with the SGR attributes active at its position. */
export interface AnsiSegment {
	text: string;
	/** CSS color value, undefined = inherit (the terminal default). */
	fg?: string;
	bg?: string;
	bold?: boolean;
	dim?: boolean;
	italic?: boolean;
	underline?: boolean;
}

/** Standard 8-color palette, mapped to the design system's semantic tokens
 *  so colored shell output feels native to the console rather than a
 *  foreign terminal palette. Bright variants are slightly lighter. */
const FG_COLORS: Record<number, string> = {
	30: 'var(--text-secondary)', // black → readable gray on dark bg
	31: 'var(--state-error-text)', // red
	32: 'var(--state-done-text)', // green
	33: 'var(--state-running-text)', // yellow
	34: 'var(--syntax-key)', // blue
	35: 'var(--syntax-keyword)', // magenta
	36: 'var(--syntax-fn)', // cyan
	37: 'var(--text-primary)', // white
	90: 'var(--text-tertiary)', // bright black (gray)
	91: '#ff8a8a', // bright red
	92: '#8fdca0', // bright green
	93: '#ffd47e', // bright yellow
	94: '#9bb8e8', // bright blue
	95: '#dda8e8', // bright magenta
	96: '#8ad8e8', // bright cyan
	97: '#ffffff' // bright white
};

const BG_COLORS: Record<number, string> = {
	40: 'var(--canvas-float)',
	41: 'rgba(224, 82, 82, 0.25)',
	42: 'rgba(61, 155, 92, 0.25)',
	43: 'rgba(232, 168, 56, 0.25)',
	44: 'rgba(123, 155, 216, 0.25)',
	45: 'rgba(201, 143, 212, 0.25)',
	46: 'rgba(123, 196, 216, 0.25)',
	47: 'rgba(255, 255, 255, 0.12)'
};

/** Parse a string with ANSI escape sequences into styled segments.
 *  Text outside any SGR influence is emitted as a single unstyled segment. */
export function parseAnsi(input: string): AnsiSegment[] {
	const segments: AnsiSegment[] = [];
	// Active SGR state, mutated as escape sequences are consumed.
	let fg: string | undefined;
	let bg: string | undefined;
	let bold = false;
	let dim = false;
	let italic = false;
	let underline = false;

	// Current unstyled/escape-free text accumulator.
	let buf = '';
	/** Flush the accumulator as a segment with the current attributes. */
	function flush() {
		if (!buf) return;
		const seg: AnsiSegment = { text: buf };
		if (fg) seg.fg = fg;
		if (bg) seg.bg = bg;
		if (bold) seg.bold = true;
		if (dim) seg.dim = true;
		if (italic) seg.italic = true;
		if (underline) seg.underline = true;
		segments.push(seg);
		buf = '';
	}

	let i = 0;
	while (i < input.length) {
		// ESC (0x1B): start of an escape sequence.
		if (input.charCodeAt(i) === 0x1b && i + 1 < input.length) {
			const next = input[i + 1];
			if (next === '[') {
				// CSI: ESC [ params final-byte
				const end = input.indexOf('m', i + 2);
				if (end === -1) {
					// No SGR terminator — check for other CSI finals to skip.
					const csiMatch = /^\u001b\[([0-9;?]*)([a-zA-Z])/.exec(input.slice(i));
					if (csiMatch) {
						i += csiMatch[0].length;
						continue;
					}
					// Unterminated — treat the ESC as literal text.
					buf += input[i];
					i++;
					continue;
				}
				flush();
				const params = input
					.slice(i + 2, end)
					.split(';')
					.map((p) => (p === '' ? 0 : parseInt(p, 10)));
				for (const code of params) {
					if (code === 0) {
						fg = bg = undefined;
						bold = dim = italic = underline = false;
					} else if (code === 1) bold = true;
					else if (code === 2) dim = true;
					else if (code === 3) italic = true;
					else if (code === 4) underline = true;
					else if (code === 22) {
						bold = false;
						dim = false;
					} else if (code === 23) italic = false;
					else if (code === 24) underline = false;
					else if (code === 39) fg = undefined;
					else if (code === 49) bg = undefined;
					else if (FG_COLORS[code]) fg = FG_COLORS[code];
					else if (BG_COLORS[code]) bg = BG_COLORS[code];
					// Unsupported codes (blink, strikethrough, 256-color, RGB): ignored.
				}
				i = end + 1;
				continue;
			}
			if (next === ']') {
				// OSC: ESC ] ... BEL (or ST) — hyperlinks, window titles. Strip.
				const bel = input.indexOf('\u0007', i + 2);
				if (bel !== -1) {
					i = bel + 1;
					continue;
				}
				// Unterminated — skip the ESC.
				i++;
				continue;
			}
			// Other escape (ESC alone, ESC ( charset, etc.): strip.
			i += 2;
			continue;
		}
		buf += input[i];
		i++;
	}
	flush();
	return segments;
}
