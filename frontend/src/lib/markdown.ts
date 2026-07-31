import { browser } from '$app/environment';
import { Marked } from 'marked';
import hljs from 'highlight.js/lib/common';
import DOMPurify from 'dompurify';

/** Markdown renderer with synchronous syntax highlighting + a language
 *  badge on every fenced block. Built once at module load so highlight.js
 *  language defs register a single time. `gfm` is on by default in marked
 *  v18 — that's what enables pipe tables.
 *
 *  We use a custom `code` renderer (not marked-highlight) because we need the
 *  *resolved* language for the badge: when a fence has no tag we run
 *  `highlightAuto`, whose result carries the detected language — info the
 *  highlight-only plugin throws away. The emitted markup is a wrapper holding
 *  a label + the usual <pre><code>; hljs output is pre-escaped and the whole
 *  thing is DOMPurify-sanitized downstream. */
const mdRenderer = {
	code({ text, lang }: { text: string; lang?: string }) {
		const tag = (lang ?? '').trim().split(/\s+/)[0];
		let label: string;
		let html: string;
		if (tag && hljs.getLanguage(tag)) {
			label = tag;
			html = hljs.highlight(text, { language: tag, ignoreIllegals: true }).value;
		} else {
			const auto = hljs.highlightAuto(text);
			label = auto.language ?? 'text';
			html = auto.value;
		}
		return (
			`<div class="code-block">` +
			`<div class="code-lang">${escapeHtml(label)}</div>` +
			`<pre><code class="hljs language-${escapeHtml(label)}">${html}</code></pre>` +
			`</div>`
		);
	}
};
const md = new Marked();
md.use({ renderer: mdRenderer });
// User messages get the same renderer plus GFM line breaks: chat input is
// not a markdown document, so a single newline from the textarea must stay
// a line break (GitHub-comment behaviour) instead of collapsing per strict
// markdown semantics.
const mdUser = new Marked({ breaks: true });
mdUser.use({ renderer: mdRenderer });

export function renderMarkdown(text: string): string {
	if (!browser) return escapeHtml(text);
	const raw = md.parse(text, { async: false }) as string;
	return DOMPurify.sanitize(raw);
}

/** Same pipeline as renderMarkdown but with `breaks: true` — see mdUser. */
export function renderUserMarkdown(text: string): string {
	if (!browser) return escapeHtml(text);
	const raw = mdUser.parse(text, { async: false }) as string;
	return DOMPurify.sanitize(raw);
}

export function escapeHtml(s: string): string {
	return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
