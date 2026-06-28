// Offline UI screenshot tool. Drives the system (nix) Chromium via
// playwright-core — no Playwright browser download — against a `vite preview`
// of the built SPA, with every `/api/*` call mocked from fixtures so pages that
// normally need a live gateway (session list, conversation stream) render with
// representative data offline.
//
// Usage:  node scripts/shot.mjs           # build must exist (npm run build)
//         just shot                        # builds + shoots in one step
//
// Output: design-demos/shots/*.png (light + dark, list + conversation).
//
// Why this exists: there is no display in dev, and the conversation page needs
// a provider+key to get real data. This lets us *see* layout/visual regressions
// (e.g. the scrollbar-position bug) without a running backend. See
// doc/frontend.md and frontend/DESIGN.md.

import { execFileSync, spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { chromium } from 'playwright-core';

const here = dirname(fileURLToPath(import.meta.url));
const frontend = resolve(here, '..');
const outDir = resolve(frontend, '../design-demos/shots');
const PORT = 4319;
const BASE = `http://127.0.0.1:${PORT}`;

/** Locate the system Chromium. Prefer `CHROMIUM_BIN` (set by the nix devShell),
 *  fall back to PATH. The nix store path is not stable across rebuilds, so we
 *  never hardcode it. */
function chromiumPath() {
	if (process.env.CHROMIUM_BIN) return process.env.CHROMIUM_BIN;
	try {
		return execFileSync('which', ['chromium']).toString().trim();
	} catch {
		throw new Error('chromium not found — set CHROMIUM_BIN or run inside the nix dev shell');
	}
}

// ── Fixtures ────────────────────────────────────────────────────────────────

const NOW = Date.now();
const iso = (msAgo) => new Date(NOW - msAgo).toISOString();

/** Session metadata, keyed by id. */
const META = {
	'01J5SESSIONAAAAAAAAAAAAAAA': {
		id: '01J5SESSIONAAAAAAAAAAAAAAA',
		profile_id: 'coding-agent',
		created_at: iso(2 * 3600_000),
		workspace: '/home/duskgrow/project/rust/ominiforge',
		origin: { kind: 'new', parent_id: null, fork_at_seq: null }
	},
	'01J5SESSIONBBBBBBBBBBBBBBB': {
		id: '01J5SESSIONBBBBBBBBBBBBBBB',
		profile_id: 'coding-agent',
		created_at: iso(26 * 3600_000),
		workspace: '/home/duskgrow/project/rust/ominiforge',
		origin: { kind: 'fork', parent_id: '01J5SESSIONAAAAAAAAAAAAAAA', fork_at_seq: 12 }
	},
	'01J5SESSIONCCCCCCCCCCCCCCC': {
		id: '01J5SESSIONCCCCCCCCCCCCCCC',
		profile_id: 'research',
		created_at: iso(5 * 60_000),
		workspace: null,
		origin: { kind: 'new', parent_id: null, fork_at_seq: null }
	}
};

/** Derived summaries, keyed by id. `first_user_input` drives the list title. */
const SUMMARY = {
	'01J5SESSIONAAAAAAAAAAAAAAA': {
		total_turns: 12,
		total_model_requests: 14,
		total_tool_calls: 5,
		total_tool_failures: 0,
		total_input_tokens: 48000,
		total_output_tokens: 6200,
		total_cache_read_tokens: 31000,
		cache_hit_rate: 0.64,
		cost_usd: 0.0312,
		first_user_input: '修复 auth 中间件 token 过期边界 bug aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
		tools_used: { read: 3, edit: 1, bash: 1 },
		errors: {}
	},
	'01J5SESSIONBBBBBBBBBBBBBBB': {
		total_turns: 3,
		total_model_requests: 3,
		total_tool_calls: 1,
		total_tool_failures: 0,
		total_input_tokens: 9000,
		total_output_tokens: 1100,
		total_cache_read_tokens: 4000,
		cache_hit_rate: 0.44,
		cost_usd: 0.004,
		first_user_input: '换个思路：把校验改成事件流校验而不是配置层',
		tools_used: { read: 1 },
		errors: {}
	},
	'01J5SESSIONCCCCCCCCCCCCCCC': {
		total_turns: 1,
		total_model_requests: 1,
		total_tool_calls: 0,
		total_tool_failures: 0,
		total_input_tokens: 1200,
		total_output_tokens: 300,
		total_cache_read_tokens: 0,
		cache_hit_rate: 0,
		cost_usd: null,
		first_user_input: null,
		tools_used: {},
		errors: {}
	}
};

/** Config-layer runtime per session. Session A is configured for `sonnet`; its
 *  event log below runs `haiku` on one request, so the RUNTIME panel must show
 *  the B4 divergence marker. */
const RUNTIME = {
	'01J5SESSIONAAAAAAAAAAAAAAA': { provider: 'anthropic', model: 'sonnet', env: ['nix', 'cargo'] },
	'01J5SESSIONBBBBBBBBBBBBBBB': { provider: 'anthropic', model: 'sonnet', env: ['nix', 'cargo'] },
	'01J5SESSIONCCCCCCCCCCCCCCC': { provider: 'anthropic', model: 'haiku', env: [] }
};

/** A committed event envelope. */
function evt(seq, payload, sourceKind = 'Model') {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: '01J5SESSIONAAAAAAAAAAAAAAA',
		timestamp: iso(2 * 3600_000 - seq * 1000),
		source: { kind: sourceKind, id: 'fixture' },
		parent_event_id: null,
		turn_id: 't1',
		payload
	};
}

/** Event log for the conversation page: exercises user / reasoning / text /
 *  tool / and a model divergence (haiku on a sonnet-configured session). */
const EVENTS = [
	evt(0, { Turn: { Started: { turn_id: 't1', input: '修复 auth 中间件里 token 过期检查的边界 bug' } } }, 'Runtime'),
	evt(1, { Model: { RequestStarted: { request_id: 'r1', provider: 'anthropic', model: 'haiku', temperature: 0, max_tokens: null, tool_schemas_count: 8, input_tokens_estimate: 1200 } } }),
	evt(2, { Model: { ContentBlock: { request_id: 'r1', index: 0, content: { Reasoning: { text: '先定位过期判断。`expires_at < now` 在相等时刻把 token 判成过期，应该是 `<=` 的反面——即 `now > expires_at` 才算过期。' } } } } }),
	evt(3, { Model: { ContentBlock: { request_id: 'r1', index: 1, content: { Text: { text: '问题在 `auth/middleware.rs`：过期检查写成了 `expires_at < now`，边界时刻（`expires_at == now`）会被误判。改用 `now > expires_at`。\n\n```rust\nif now > token.expires_at {\n    return Err(AuthError::Expired);\n}\n```' } } } } }),
	evt(4, { Model: { ContentBlock: { request_id: 'r1', index: 2, content: { ToolCall: { id: 'c1', name: 'edit', arguments: '{"file_path":"src/auth/middleware.rs","old":"expires_at < now","new":"now > expires_at"}' } } } } }),
	evt(5, { Tool: { Completed: { tool_call_event_id: { session_id: '01J5SESSIONAAAAAAAAAAAAAAA', seq: 4 }, result: { content: [{ Text: 'edited 1 file' }], is_error: false, error_code: null }, duration_ms: 12, output_bytes: 12, artifacts_created: [] } } }),
	evt(6, { Model: { ContentBlock: { request_id: 'r1', index: 3, content: { Text: { text: '已修复。边界时刻的 token 现在不再被误判为过期。' } } } } }),
	// Padding turns to force the stream to overflow vertically, so screenshots
	// surface scrollbar placement (the regression this tool was built to catch).
	...Array.from({ length: 6 }, (_, k) =>
		evt(7 + k, {
			Model: {
				ContentBlock: {
					request_id: 'r1',
					index: 4 + k,
					content: { Text: { text: `补充说明 ${k + 1}：又一段较长的回复文本，用来把对话流撑高、逼出竖向滚动条，从而在截图里检查滚动条是否贴在视口最右侧而不是跑到中间。` } }
				}
			}
		})
	)
];

// ── Mock wiring ───────────────────────────────────────────────────────────

/** Route every `/api/*` request to a fixture. SSE is fulfilled as a single
 *  body of committed `data:` frames; the transport reads them then the stream
 *  ends (no live deltas needed for a static screenshot). */
async function mockApi(page) {
	await page.route('**/api/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname.replace(/^\/api/, '');
		const json = (obj) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(obj) });

		if (path === '/sessions') return json({ sessions: Object.keys(META) });

		const m = path.match(/^\/sessions\/([^/]+)(\/.*)?$/);
		if (m) {
			const id = decodeURIComponent(m[1]);
			const sub = m[2] ?? '';
			if (sub === '') return json(META[id] ?? META['01J5SESSIONAAAAAAAAAAAAAAA']);
			if (sub === '/summary') return json(SUMMARY[id] ?? SUMMARY['01J5SESSIONCCCCCCCCCCCCCCC']);
			if (sub === '/runtime') return json(RUNTIME[id] ?? RUNTIME['01J5SESSIONCCCCCCCCCCCCCCC']);
			if (sub === '/events') {
				const body = EVENTS.map((e) => `id: ${e.seq}\ndata: ${JSON.stringify(e)}\n\n`).join('');
				return route.fulfill({ status: 200, contentType: 'text/event-stream', body });
			}
		}
		return route.fulfill({ status: 404, contentType: 'application/json', body: '{"error":"unmocked"}' });
	});
}

// ── Server lifecycle ────────────────────────────────────────────────────────

function startPreview() {
	const proc = spawn('npm', ['run', 'preview', '--', '--port', String(PORT), '--strictPort'], {
		cwd: frontend,
		stdio: 'ignore'
	});
	return proc;
}

async function waitForServer(timeoutMs = 20000) {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		try {
			const res = await fetch(BASE);
			if (res.ok) return;
		} catch {
			// not up yet
		}
		await new Promise((r) => setTimeout(r, 250));
	}
	throw new Error(`preview server did not start on ${BASE}`);
}

// ── Overflow diagnosis ───────────────────────────────────────────────────────

/** Detect unwanted horizontal overflow — both at the document level (a stray
 *  page-wide horizontal scrollbar) AND inside any scroll container (an element
 *  whose `scrollWidth` exceeds its `clientWidth`). The list-card regression was
 *  the *internal* kind: `.page` (overflow:auto, max-width:880) held content
 *  1303px wide, so it scrolled horizontally and shoved its vertical scrollbar
 *  inward — invisible to a document-level check. Reports the offending elements
 *  with the CSS that over-sized them, so we fix the real culprit. Returns true
 *  when any unwanted horizontal overflow exists. */
async function diagnoseOverflow(page, label) {
	const report = await page.evaluate(() => {
		const docW = document.documentElement.clientWidth;
		const out = {
			viewport: window.innerWidth,
			docClientWidth: docW,
			docScrollWidth: document.documentElement.scrollWidth,
			docOverflow: document.documentElement.scrollWidth > docW + 1,
			cards: document.querySelectorAll('.card').length,
			widestTitle: Math.max(0, ...[...document.querySelectorAll('.card-title')].map((t) => t.scrollWidth)),
			containers: [] // scroll containers overflowing their own width
		};
		for (const el of Array.from(document.querySelectorAll('*'))) {
			const cs = getComputedStyle(el);
			const scrolls = cs.overflowX === 'auto' || cs.overflowX === 'scroll';
			// A scroll container whose content is wider than its box → a horizontal
			// scrollbar appears inside it. Tolerate code blocks (<pre>), which are
			// *meant* to scroll horizontally.
			if (scrolls && el.scrollWidth > el.clientWidth + 1 && el.tagName !== 'PRE') {
				out.containers.push({
					tag: el.tagName.toLowerCase(),
					cls: (el.className || '').toString().slice(0, 40),
					clientW: el.clientWidth,
					scrollW: el.scrollWidth,
					maxWidth: cs.maxWidth
				});
			}
		}
		return out;
	});
	const bad = report.docOverflow || report.containers.length > 0;
	console.log(`\n[diagnose ${label}] viewport=${report.viewport} docClientW=${report.docClientWidth} docScrollW=${report.docScrollWidth} docOverflow=${report.docOverflow} internalOverflow=${report.containers.length > 0} cards=${report.cards} widestCardTitleScrollW=${report.widestTitle}`);
	for (const c of report.containers.slice(0, 8)) {
		console.log(`  · internal overflow <${c.tag}.${c.cls}> clientW=${c.clientW} scrollW=${c.scrollW} maxWidth=${c.maxWidth}`);
	}
	if (!bad) console.log('  (no unwanted horizontal overflow)');
	return bad;
}

// ── Shoot ─────────────────────────────────────────────────────────────────

const CHECK = process.argv.includes('--check');

async function shoot() {
	mkdirSync(outDir, { recursive: true });
	const browser = await chromium.launch({ executablePath: chromiumPath() });

	const targets = [
		{ name: 'sessions-list', path: '/sessions', waitFor: '.card, .muted', contentSel: '.card' },
		{
			name: 'conversation',
			path: '/sessions/01J5SESSIONAAAAAAAAAAAAAAA',
			waitFor: '.item-text, .conv-inner',
			contentSel: '.item-text'
		}
	];

	// Widths to assert no horizontal overflow at. Narrow widths matter most: the
	// list-card overflow regression only bit below ~1440px (a long, nowrap title
	// pushed the card past the 880px page). Wide widths masked it.
	const checkWidths = [1366, 1280, 1024];
	const overflows = [];

	// Visual screenshots for human review (wide viewport, both themes).
	for (const theme of ['dark', 'light']) {
		for (const t of targets) {
			const page = await browser.newPage({ viewport: { width: 1680, height: 900 } });
			await page.addInitScript((th) => localStorage.setItem('theme', th), theme);
			await mockApi(page);
			await page.goto(`${BASE}${t.path}`, { waitUntil: 'networkidle' });
			await page.waitForSelector(t.waitFor, { timeout: 5000 }).catch(() => {});
			await page.waitForTimeout(400); // settle fonts/animations
			const file = resolve(outDir, `${t.name}-${theme}.png`);
			await page.screenshot({ path: file });
			console.log(`shot: ${file}`);
			await page.close();
		}
	}

	// Overflow regression gate: assert no page scrolls horizontally at any of the
	// check widths. Runs in the default (dark) theme — overflow is layout, not
	// colour, so one theme suffices.
	for (const t of targets) {
		for (const w of checkWidths) {
			const page = await browser.newPage({ viewport: { width: w, height: 880 } });
			await mockApi(page);
			await page.goto(`${BASE}${t.path}`, { waitUntil: 'networkidle' });
			// Wait for *content* (not the loading/empty `.muted`) so the gate measures
			// the populated page — the list fetches its cards after hydration.
			await page.waitForSelector(t.contentSel, { timeout: 8000 }).catch(() => {});
			await page.waitForTimeout(500);
			const bad = await diagnoseOverflow(page, `${t.name}@${w}`);
			if (bad) overflows.push(`${t.name}@${w}`);
			await page.close();
		}
	}

	await browser.close();
	return overflows;
}

// ── Main ─────────────────────────────────────────────────────────────────

const preview = startPreview();
let overflows = [];
try {
	await waitForServer();
	overflows = await shoot();
} finally {
	preview.kill('SIGTERM');
}

if (overflows.length) {
	console.error(`\n✗ horizontal overflow at: ${overflows.join(', ')}`);
	if (CHECK) process.exit(1);
} else {
	console.log('\n✓ no horizontal overflow at any checked width');
}
