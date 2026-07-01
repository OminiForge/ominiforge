// Syntax-highlight helpers for tool-result rendering. Shared by CodeView (read)
// and any result body that shows source. Uses the same `highlight.js/lib/common`
// bundle the markdown renderer registers, so no extra language weight.
import hljs from 'highlight.js/lib/common';

/** Map a file extension to a highlight.js language name; undefined = unknown
 *  (caller falls back to highlightAuto). Kept small: the languages this repo and
 *  its docs actually contain. */
const EXT_LANG: Record<string, string> = {
	rs: 'rust',
	ts: 'typescript',
	tsx: 'typescript',
	js: 'javascript',
	jsx: 'javascript',
	mjs: 'javascript',
	cjs: 'javascript',
	py: 'python',
	go: 'go',
	java: 'java',
	c: 'c',
	h: 'c',
	cpp: 'cpp',
	cc: 'cpp',
	hpp: 'cpp',
	rb: 'ruby',
	php: 'php',
	sh: 'bash',
	bash: 'bash',
	zsh: 'bash',
	json: 'json',
	yaml: 'yaml',
	yml: 'yaml',
	toml: 'ini',
	ini: 'ini',
	md: 'markdown',
	html: 'xml',
	xml: 'xml',
	svelte: 'xml',
	css: 'css',
	sql: 'sql',
	lua: 'lua',
	kt: 'kotlin',
	swift: 'swift'
};

/** highlight.js language for a path, or undefined when the extension is unknown
 *  or unsupported by the loaded bundle. */
export function langFromPath(path: string): string | undefined {
	const ext = path.split('.').pop()?.toLowerCase() ?? '';
	const lang = EXT_LANG[ext];
	return lang && hljs.getLanguage(lang) ? lang : undefined;
}

/** Highlight a whole code block at once (keeps multi-line hljs context, unlike
 *  per-line highlighting which breaks constructs across newlines) using the
 *  language guessed from `path`, or auto-detection. Output is hljs's pre-escaped
 *  markup — safe to drop into `{@html}`. */
export function highlightBlock(code: string, path: string): string {
	const lang = langFromPath(path);
	return lang
		? hljs.highlight(code, { language: lang, ignoreIllegals: true }).value
		: hljs.highlightAuto(code).value;
}
