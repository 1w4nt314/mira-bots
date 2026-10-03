// Truth table for src/lib/inbox.ts: compiles it with the repo's TypeScript, runs it in node.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/inbox.ts", import.meta.url), "utf8");
// Only type imports are allowed (the file must run on its own): a runtime import fails here.
assert.ok(!/^import\s+(?!type\b)/m.test(src), "inbox.ts must only have type imports");
const out = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const file = join(mkdtempSync(join(tmpdir(), "inbox-")), "inbox.mjs");
writeFileSync(file, out.outputText);
const x = await import(pathToFileURL(file).href);

let n = 0;
const check = (actual, want, msg) => {
  assert.deepEqual(actual, want, msg);
  n++;
};

const item = (o = {}) => ({
  id: "i1", kind: "github", externalId: "github:o/r#1", sourceId: "github:o/r", title: "Fejl", hasBody: false,
  labels: [], url: null, number: 1, repo: "o/r", path: null, author: null, project: null, candidates: [],
  updatedAt: null, seenAt: 1000, state: "new", ticketId: null, notes: [], ticketKind: null, duplicateOf: null,
  ...o,
});
const wb = (o = {}) => ({
  comment: "none", close: "none", commentUrl: null, commentedAt: null, closedAt: null, attempts: 0, lastError: null, lastBody: null,
  ...o,
});
const ext = (o = {}) => ({
  kind: "github", externalId: "github:o/r#7", repo: "o/r", number: 7, path: null, url: null, title: "t", labels: [],
  author: null, notes: [], inboxItemId: "i1", importedAt: 0, writeBack: wb(), ...o,
});
const status = (o = {}) => ({ refreshing: false, lastRefreshAt: null, sources: [], ...o });
const source = (o = {}) => ({
  id: "github:o/r", kind: "github", label: "o/r", project: null, lastFetchAt: null, ok: true, error: null,
  errorKind: null, nextRetryAt: null, items: 0, capped: false, notes: [], ...o,
});

// inboxLabel
check(x.inboxLabel(null), null);
check(x.inboxLabel(ext()), "GitHub #7");
check(x.inboxLabel(ext({ number: null })), "GitHub");
check(x.inboxLabel(ext({ kind: "folder", number: null, path: "a.md" })), "indbakke");
check(x.sourceBadge("github"), "GitHub");
check(x.sourceBadge("folder"), "fil");

// sourceLine / sourceIdText (labels of the source id)
check(x.sourceLine(item()), "GitHub #1 i o/r");
check(x.sourceLine(item({ number: null, repo: null })), "GitHub");
check(x.sourceLine(item({ kind: "folder", path: "fejl-1.md", number: null, repo: null })), "fil fejl-1.md");
check(x.sourceIdText({ sourceId: "github:o/r", repo: "O/R" }), "GitHub O/R");
check(x.sourceIdText({ sourceId: "github:o/r[bug,ui]", repo: "o/r" }), "GitHub o/r [bug, ui]");
check(x.sourceIdText({ sourceId: "github:o/r[bug]", repo: null }), "GitHub o/r [bug]");
check(x.sourceIdText({ sourceId: "folder:_rod", repo: null }), "indbakke");
check(x.sourceIdText({ sourceId: "folder:web", repo: null }), "indbakke i web");
check(x.sourceIdText({ sourceId: "other", repo: null }), "other");

// writeBackBadge
check(x.writeBackBadge(ext()), null);
check(x.writeBackBadge(ext({ writeBack: wb({ comment: "inflight" }) })).text, "melder tilbage…");
const done = x.writeBackBadge(ext({ writeBack: wb({ comment: "done", commentUrl: "https://github.com/o/r/issues/7#c1" }) }));
check([done.text, done.title], ["meldt tilbage ✓", "https://github.com/o/r/issues/7#c1"]);
const failed = x.writeBackBadge(ext({ writeBack: wb({ comment: "failed", lastError: "GitHub: rate limit" }) }));
check([failed.text, failed.title], ["ikke meldt tilbage", "GitHub: rate limit"]);
check(x.writeBackBadge(ext({ writeBack: wb({ comment: "failed" }) })).title.length > 0, true);

// grouping and sorting: newest first (updatedAt, else seenAt), ties by title, never mutates
const items = [
  item({ id: "a", title: "B", seenAt: 10 }),
  item({ id: "b", title: "A", seenAt: 10 }),
  item({ id: "c", title: "C", updatedAt: "2026-10-01T10:00:00Z", seenAt: 5 }),
  item({ id: "d", state: "started", seenAt: 1 }),
  item({ id: "e", state: "dismissed", seenAt: 2 }),
  item({ id: "f", updatedAt: "ikke en dato", seenAt: 99_999_999_999_999, title: "Z" }),
];
const copy = JSON.stringify(items);
const g = x.groupInbox(items);
check(g.new.map((i) => i.id), ["f", "c", "b", "a"]);
check(g.started.map((i) => i.id), ["d"]);
check(g.dismissed.map((i) => i.id), ["e"]);
check(JSON.stringify(items), copy, "groupInbox must not mutate its input");
check(x.sortInbox([item({ id: "z", seenAt: 1 }), item({ id: "y", seenAt: 1 })]).map((i) => i.id), ["y", "z"]);

// matchesInboxFilter
check(x.matchesInboxFilter({ project: null }, "all"), true);
check(x.matchesInboxFilter({ project: null }, "none"), true);
check(x.matchesInboxFilter({ project: "web" }, "none"), false);
check(x.matchesInboxFilter({ project: "Web" }, { id: "web" }), true);
check(x.matchesInboxFilter({ project: "api" }, { id: "web" }), false);
check(x.matchesInboxFilter({ project: null }, { id: "web" }), false);
check(x.matchesInboxFilter({ project: null, candidates: ["api", "WEB"] }, { id: "web" }), true);

// labels, duplicate text, empty text, start button
check(x.visibleLabels(["a", "b"]), { shown: ["a", "b"], extra: 0 });
check(x.visibleLabels(["a", "b", "c", "d", "e", "f", "g"]), { shown: ["a", "b", "c", "d", "e"], extra: 2 });
check(x.duplicateText({ shortId: "ab12", title: "Fejl" }), "Ligner ticket ab12: «Fejl»");
check(x.inboxEmptyText("all"), "Ingen nye emner i indbakken");
check(x.inboxEmptyText({ id: "web" }), "Ingen nye emner i indbakken i dette projekt");
check(x.startButtonText(null, false, false), "Start");
check(x.startButtonText({ kind: "agent", agentName: "coder-01" }, false, true), "Start og tildel til coder-01");
check(x.startButtonText({ kind: "empty", seatKind: "work" }, false, false), "Start og start agent");
check(x.startButtonText(null, true, true), "Henter fra GitHub…");

// status texts (clock = local time)
const at = new Date(2026, 9, 3, 7, 5).getTime();
check(x.clockText(at), "07:05");
check(x.fetchStatusText(status({ refreshing: true, lastRefreshAt: at })), "Henter…");
check(x.fetchStatusText(status({ lastRefreshAt: at })), "Seneste hentning kl. 07:05");
check(x.fetchStatusText(status()), "Ikke hentet endnu");
check(x.sourceErrors(status({ sources: [source(), source({ id: "github:x/y", label: "x/y", ok: false, error: "gh ikke fundet — Indbakke fra GitHub er slået fra (mappe-kilden virker)" })] })),
  [{ id: "github:x/y", label: "x/y", error: "gh ikke fundet — Indbakke fra GitHub er slået fra (mappe-kilden virker)" }]);
check(x.cappedHint(status({ sources: [source()] })), null);
check(x.cappedHint(status({ sources: [source(), source({ capped: true })] })), "Højst 100 åbne issues pr. repo vises.");
check(x.sourceNotes(status({ sources: [source({ notes: ["n1"] }), source({ notes: ["n2", "n3"] })] })), ["n1", "n2", "n3"]);
check(x.showInboxSection(0, null), false);
check(x.showInboxSection(0, status({ sources: [source()] })), false);
check(x.showInboxSection(2, null), true);
check(x.showInboxSection(0, status({ sources: [source({ ok: false, error: "x" })] })), true);

// polling rules: floors 60 s (timer/visible), 15 s (focus), 5 s (manual)
const t0 = 1_000_000;
check(x.canRefresh(null, 5000, t0), true);
check(x.canRefresh(t0, 5000, t0 + 4999), false);
check(x.canRefresh(t0, 5000, t0 + 5000), true);
check(x.pollReason("mount", false, t0, t0 + 1), "startup");
check(x.pollReason("mount", true, null, t0), "startup");
check(x.pollReason("timer", true, t0, t0 + 60_000), "timer");
check(x.pollReason("timer", true, t0, t0 + 59_500), "timer"); // timer jitter: within the slack
check(x.pollReason("timer", true, t0, t0 + 30_000), null);
check(x.pollReason("timer", false, t0, t0 + 600_000), null); // hidden window never polls
check(x.pollReason("timer", true, null, t0), "timer");
check(x.pollReason("focus", true, t0, t0 + 14_999), null);
check(x.pollReason("focus", true, t0, t0 + 15_000), "focus");
check(x.pollReason("focus", false, t0, t0 + 600_000), null);
check(x.pollReason("visible", true, t0, t0 + 59_999), null);
check(x.pollReason("visible", true, t0, t0 + 60_000), "focus");
check(x.pollReason("visible", false, null, t0), null);
check(x.pollReason("manual", false, t0, t0 + 5_000), "manual"); // the button works whatever the visibility says
check(x.pollReason("manual", true, t0, t0 + 4_999), null);
check([x.POLL_INTERVAL_MS, x.FOCUS_FLOOR_MS, x.MANUAL_FLOOR_MS], [60_000, 15_000, 5_000]);

// drag ids
check(x.inboxDragId("abc"), "inbox:abc");
check(x.draggedInboxId("inbox:abc"), "abc");
check(x.draggedInboxId("inbox:"), null);
check(x.draggedInboxId("ticket:abc"), null);
check(x.draggedInboxId("agent:abc"), null);
check(x.draggedInboxId(null), null);
check(x.draggedInboxId(42), null);

console.log(`test-inbox: ${n} cases ok`);
