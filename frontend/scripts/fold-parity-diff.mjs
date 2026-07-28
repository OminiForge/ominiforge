// Diff the client-side fold baseline against the backend view fold.
// Usage: node scripts/fold-parity-diff.mjs <baseline.json> <rust-view.json>
import { readFileSync } from 'node:fs';

const [baselinePath, rustPath] = process.argv.slice(2);
const base = JSON.parse(readFileSync(baselinePath, 'utf8'));
const rust = JSON.parse(readFileSync(rustPath, 'utf8'));

console.log('baseline items:', base.items.length, ' rust items:', rust.items.length);
console.log('baseline lastSeq:', base.lastSeq, ' rust last_seq:', rust.last_seq);

// Derived state: turnRunning + runtimeModels must match too — a view that
// folds items identically but loses the turn-running flag still breaks the
// live indicator / Cancel affordance / send-queueing.
const derivedOk =
	(base.turnRunning ?? false) === (rust.turn_running ?? false) &&
	JSON.stringify([...(base.runtimeModels ?? [])]) ===
		JSON.stringify([...(rust.runtime_models ?? [])]);
console.log(
	'derived state: turnRunning',
	base.turnRunning ?? false,
	'vs',
	rust.turn_running ?? false,
	'· runtimeModels',
	JSON.stringify(base.runtimeModels),
	'vs',
	JSON.stringify(rust.runtime_models),
	derivedOk ? 'OK' : 'MISMATCH'
);

// Compare semantic fields only: internal ids and the transient `streaming`
// flag are rendering concerns, not fold output. Keys are sorted so the
// comparison is insensitive to serializer key order (TS vs serde).
function norm(items) {
	return items.map((i) => {
		const { id: _id, streaming: _s, ...rest } = i;
		return Object.fromEntries(Object.entries(rest).sort(([x], [y]) => x.localeCompare(y)));
	});
}
const a = norm(base.items);
const b = norm(rust.items);
const ea = JSON.stringify(a);
const eb = JSON.stringify(b);
console.log('normalized equal:', ea === eb);
if (ea !== eb) {
	const n = Math.max(a.length, b.length);
	let shown = 0;
	for (let i = 0; i < n && shown < 5; i++) {
		if (JSON.stringify(a[i]) !== JSON.stringify(b[i])) {
			console.log(`--- diff at item ${i} ---`);
			console.log('baseline:', JSON.stringify(a[i])?.slice(0, 400));
			console.log('rust    :', JSON.stringify(b[i])?.slice(0, 400));
			shown++;
		}
	}
	process.exit(1);
}
if (!derivedOk) {
	console.log('derived state mismatch');
	process.exit(1);
}
