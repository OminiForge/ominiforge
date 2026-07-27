/** Tree-sitter syntax highlighting for tool-result rendering.
 *  Replaces highlight.js with a real parser: better accuracy, incremental
 *  updates, and direct mapping to the design tokens (`--syntax-*`).
 *
 *  Uses `web-tree-sitter` (WASM). Grammars and queries are NOT bundled —
 *  bundling can't enumerate every language and would be tens of MB. They're
 *  downloaded on demand from a CDN the first time a language is highlighted,
 *  then cached in memory (and by the browser's HTTP cache). The only bundled
 *  asset is the ~200KB runtime `web-tree-sitter.wasm`. Parsed trees live in a
 *  true LRU cache (each `Tree` is a WASM heap object, `.delete()`d on evict).
 *  The parser itself is a singleton reused across all highlights. */

import { Parser, Language, Query, Tree } from 'web-tree-sitter';

/** CDN base for on-demand grammar WASM files (the `tree-sitter-wasms` npm
 *  package publishes one wasm per language). Downloaded lazily per language,
 *  not bundled — we can't enumerate every language at build time. */
const GRAMMAR_BASE = 'https://cdn.jsdelivr.net/npm/tree-sitter-wasms@0.1.9/out';

/** Base for on-demand highlight queries (the upstream tree-sitter repos),
 *  used only for languages whose query we don't ship locally. */
const QUERY_BASE = 'https://cdn.jsdelivr.net/gh/tree-sitter';

/** One highlight span: byte range + capture name (e.g. `keyword`, `function`). */
export interface HighlightSpan {
	start: number;
	end: number;
	capture: string;
}

/** The highlighter's cache entry: a parsed tree plus the content hash it came from. */
interface CacheEntry {
	tree: Tree;
	hash: number;
}

/** The singleton highlighter. Lazily initializes the WASM runtime and caches
 *  parsed trees per path (key = path, value = {tree, hash}). The parser is
 *  reused; grammars are loaded on first use per language. */
class TreeSitterHighlighter {
	private parser: InstanceType<typeof Parser> | null = null;
	private languages = new Map<string, Language>();
	private cache = new Map<string, CacheEntry>();
	private readonly MAX_CACHE = 50;

	/** Initialize the WASM runtime (idempotent). `locateFile` points the
	 *  Emscripten loader at the runtime `web-tree-sitter.wasm` we copied into
	 *  `static/` — without it the browser can't resolve the runtime and
	 *  `Parser.init()` rejects (the grammars alone are useless). */
	private async init(): Promise<void> {
		if (this.parser) return;
		await Parser.init({
			locateFile: (file: string) => `/${file}`
		});
		this.parser = new Parser();
	}

	/** Simple string hash for cache invalidation. */
	private hash(s: string): number {
		let h = 0;
		for (let i = 0; i < s.length; i++) {
			h = (Math.imul(31, h) + s.charCodeAt(i)) | 0;
		}
		return h;
	}

	/** Load a grammar for `lang` (e.g. `rust`, `typescript`), downloading it
	 *  on demand if it isn't already cached. Grammars are NOT bundled — that
	 *  would be tens of MB and can't enumerate every language. Instead they're
	 *  fetched lazily from a CDN the first time a file of that language is
	 *  highlighted, then kept in memory (and in the browser's HTTP cache).
	 *  Returns `null` when the language has no published grammar (plain-text
	 *  fallback). */
	private async loadLanguage(lang: string): Promise<Language | null> {
		if (this.languages.has(lang)) return this.languages.get(lang)!;
		// Serialize loads per language so two concurrent files of the same new
		// language don't both fetch the grammar.
		const pending = this.languagePending.get(lang);
		if (pending) return pending;
		const load = (async () => {
			try {
				const wasmPath = `${GRAMMAR_BASE}/tree-sitter-${lang}.wasm`;
				const language = await Language.load(wasmPath);
				this.languages.set(lang, language);
				return language;
			} catch {
				return null;
			} finally {
				this.languagePending.delete(lang);
			}
		})();
		this.languagePending.set(lang, load);
		return load;
	}
	private languagePending = new Map<string, Promise<Language | null>>();

	/** Highlight `code` in `lang`, returning spans in document order.
	 *  Falls back to `[]` when the grammar is unavailable (caller renders plain). */
	async highlight(code: string, lang: string, path?: string): Promise<HighlightSpan[]> {
		await this.init();
		if (!this.parser) return [];

		const language = await this.loadLanguage(lang);
		if (!language) return [];

		this.parser.setLanguage(language);

		const key = path ?? `__anon_${lang}`;
		const hash = this.hash(code);
		const cached = this.cache.get(key);

		let tree: Tree;
		if (cached && cached.hash === hash) {
			// Refresh recency: delete+re-set moves the entry to the end of the
			// Map's iteration order so true LRU eviction works.
			this.cache.delete(key);
			this.cache.set(key, cached);
			tree = cached.tree;
		} else {
			// Evict the least-recently-used entry when the cache is full. Map
			// iteration order is insertion order, so the first key is the oldest
			// — but a hit must refresh the entry's recency, so we delete+re-set
			// on access (see below) to keep true LRU order.
			if (this.cache.size >= this.MAX_CACHE) {
				const oldest = this.cache.keys().next().value;
				if (oldest) {
					this.cache.get(oldest)?.tree.delete();
					this.cache.delete(oldest);
				}
			}
			const parsed = this.parser.parse(code);
			if (!parsed) return [];
			tree = parsed;
			this.cache.set(key, { tree, hash });
		}

		// Query the tree for highlight captures. The query file is per-language
		// and lives in `static/tree-sitter/queries/{lang}/highlights.scm`.
		// Languages without a bundled query (cpp/kotlin/swift/ruby/php/lua/go
		// etc.) have no `highlights.scm` — `loadQuery` returns null and we fall
		// back to plain text (the grammar still parses, just no highlighting).
		const query = await this.loadQuery(lang, language);
		if (!query) return [];

		const spans: HighlightSpan[] = [];
		const matches = query.matches(tree.rootNode);
		for (const match of matches) {
			for (const capture of match.captures) {
				spans.push({
					start: capture.node.startIndex,
					end: capture.node.endIndex,
					capture: capture.name
				});
			}
		}
		// Sort by start so the renderer can emit spans in order.
		spans.sort((a, b) => a.start - b.start);
		return spans;
	}

	private queries = new Map<string, Query | null>();

	/** Load the highlight query for `lang`, downloading it on demand. Queries
	 *  are tiny text files; we keep the small set we ship locally in
	 *  `static/tree-sitter/queries/` and fall back to the upstream repo for
	 *  languages not bundled. `null` is cached too, so a language with no
	 *  published query doesn't re-fetch on every highlight. */
	private async loadQuery(lang: string, language: Language): Promise<Query | null> {
		if (this.queries.has(lang)) return this.queries.get(lang)!;
		const query = await this.fetchQuery(lang, language);
		this.queries.set(lang, query);
		return query;
	}

	private async fetchQuery(lang: string, language: Language): Promise<Query | null> {
		// Prefer the locally-bundled query (fast, version-pinned); fall back to
		// the upstream tree-sitter repo's queries for languages we don't ship.
		for (const url of [
			`/tree-sitter/queries/${lang}/highlights.scm`,
			`${QUERY_BASE}/tree-sitter-${lang}/master/queries/highlights.scm`
		]) {
			try {
				const response = await fetch(url);
				if (!response.ok) continue;
				const source = await response.text();
				return new Query(language, source);
			} catch {
				// try the next source
			}
		}
		return null;
	}

	/** Release all cached trees and the parser (call on page unload). */
	dispose(): void {
		for (const entry of this.cache.values()) {
			entry.tree.delete();
		}
		this.cache.clear();
		this.parser?.delete();
		this.parser = null;
		this.languages.clear();
		this.queries.clear();
	}
}

/** The singleton highlighter instance. */
export const highlighter = new TreeSitterHighlighter();

/** Map a file extension to a tree-sitter language name. */
export function langFromPath(path: string): string | undefined {
	const ext = path.split('.').pop()?.toLowerCase() ?? '';
	// Map the extension to a tree-sitter language name. This is NOT limited to
	// bundled grammars — any language here is downloaded on demand. Unknown
	// extensions (no grammar published) return undefined → plain-text fallback.
	const map: Record<string, string> = {
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
		cxx: 'cpp',
		hpp: 'cpp',
		rb: 'ruby',
		php: 'php',
		sh: 'bash',
		bash: 'bash',
		zsh: 'bash',
		json: 'json',
		yaml: 'yaml',
		yml: 'yaml',
		toml: 'toml',
		ini: 'ini',
		html: 'html',
		xml: 'html',
		svelte: 'svelte',
		css: 'css',
		sql: 'sql',
		lua: 'lua',
		kt: 'kotlin',
		kts: 'kotlin',
		swift: 'swift',
		vue: 'vue',
		scala: 'scala',
		ex: 'elixir',
		exs: 'elixir',
		fs: 'fsharp',
		ml: 'ocaml',
		clj: 'clojure',
		hs: 'haskell',
		dart: 'dart',
		r: 'r',
		jl: 'julia',
		zig: 'zig',
		nim: 'nim',
		v: 'v',
		sol: 'solidity',
		move: 'move',
		proto: 'proto',
		tf: 'hcl',
		hcl: 'hcl',
		nix: 'nix'
	};
	return map[ext];
}

/** Map a tree-sitter capture name to a design-token CSS class. */
export function captureToClass(capture: string): string {
	const map: Record<string, string> = {
		keyword: 'syntax-keyword',
		'keyword.function': 'syntax-keyword',
		'keyword.return': 'syntax-keyword',
		string: 'syntax-str',
		'string.special': 'syntax-str',
		number: 'syntax-num',
		boolean: 'syntax-num',
		function: 'syntax-fn',
		'function.call': 'syntax-fn',
		'function.method': 'syntax-fn',
		method: 'syntax-fn',
		type: 'syntax-type',
		'type.builtin': 'syntax-type',
		constructor: 'syntax-type',
		variable: 'syntax-key',
		'variable.builtin': 'syntax-key',
		property: 'syntax-type',
		field: 'syntax-type',
		parameter: 'syntax-key',
		comment: 'syntax-comment',
		operator: 'syntax-comment',
		punctuation: 'syntax-comment',
		constant: 'syntax-num',
		'constant.builtin': 'syntax-num',
		namespace: 'syntax-type',
		label: 'syntax-keyword',
		tag: 'syntax-keyword',
		attribute: 'syntax-type'
	};
	return map[capture] ?? 'syntax-key';
}
