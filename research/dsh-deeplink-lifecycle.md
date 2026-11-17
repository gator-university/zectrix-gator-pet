# DSH 0.2.2-rc.1 — Deep links, session lifecycle, status sources, process model

Research target: **DeepSeek Harness 0.2.2-rc.1** desktop app on this Windows machine
(`E:\Apps\DeepSeek Harness\DeepSeek Harness.exe`, Electron + `resources/app.asar`).

Sources read (read-only):

| What | Path |
|---|---|
| Extracted `dsh/` subtree of the asar | `C:\Users\liujj\Downloads\dsh-asar\dsh\` (packages in `node_modules/@deepseek-ai/`) |
| Electron main / preload / renderer (not extracted) | `E:\Apps\DeepSeek Harness\resources\app.asar` → `lib/main.js`, `lib/preload-*.cjs`, `renderer/` |
| Live profile | `C:\Users\liujj\.dsh\` (`storages/`, `sessions/`, `profiles/desktop`) |
| Registry registration | `HKCU\Software\Classes\dsh\shell\open\command` |

`lib/main.js` is only inside the asar (838 KB minified-but-commented bundle). It can be read
without extracting by parsing the asar header (see appendix A) — the asar layout is
`dsh/`, `node_modules/`, `lib/`, `renderer/`, `package.json`; only `dsh/` was extracted to
`C:\Users\liujj\Downloads\dsh-asar\dsh\`.

---

## 1. Deep link URL format

### Answer in one paragraph

`dsh://` is registered as the app's URL scheme (`app.setAsDefaultProtocolClient("dsh")`), but the
shipped code only understands **one** deep link: the literal string `dsh://open` (or `dsh://open/`),
and that match exists **only in the macOS `open-url` handler**, where it merely calls
`focusPrimaryWindow()`. On Windows the URL arrives as `argv[1]` (registry command is
`"E:\Apps\DeepSeek Harness\DeepSeek Harness.exe" "%1"`), and `lib/main.js` **never reads argv for a
URL** — the only argv inspection in the whole main process is `process.argv.includes("--updated")`.
A second launch of the exe (which is what Windows does for `dsh://…`) is swallowed by the
single-instance lock and routed to `focusPrimaryWindow`, whose handler ignores the argument list.
Therefore: **there is no supported deep link that opens/focuses a specific session, workspace, or
new session, and no way to pass a prompt.** `dsh://session/<id>`, `dsh://open/<id>`,
`dsh://workspace/<id>`, `dsh://new`, `dsh://?prompt=…` are all accepted by Windows/the OS but
perform exactly the same action as launching the exe with no arguments: focus or start the app,
which restores whatever session the renderer last had selected. The internal SPA origin is a
*different* scheme (`dsh-app://app/`), so `dsh://app/...` is not a route either.

### Evidence

Registration + the only URL handling — `lib/main.js` (app.asar):

```js
if (app.isPackaged || process.env.DSH_DESKTOP_DEV_APP === "1") app.setAsDefaultProtocolClient("dsh");
app.on("open-url", (event, url) => {
    event.preventDefault();
    if (url === "dsh://open" || url === "dsh://open/") focusPrimaryWindow();
});
app.on("activate", (_event, hasVisibleWindows) => { if (!hasVisibleWindows) focusPrimaryWindow(); });
app.on("window-all-closed", () => { if (process.platform !== "darwin") app.quit(); });
```

Windows second-launch path — `lib/main.js`, region `lib/types/single-instance.js`:

```js
function claimDesktopSingleInstance(application, focusOwner) {
    if (!application.requestSingleInstanceLock()) { application.quit(); return false; }
    application.on("second-instance", focusOwner);       // focusOwner = focusPrimaryWindow
    return true;
}
```

`focusPrimaryWindow` takes **no** parameters, so `second-instance`'s `(event, argv, cwd)` is
discarded. The whole main process contains 6 occurrences of the substring `argv`, five of which are
AppleScript `on run argv` strings and the sixth is:

```js
let raiseAfterUpdate = process.platform === "win32" && process.argv.includes("--updated");
```

The internal application scheme is *not* `dsh` — same file:

```js
/** Scheme of Desktop-owned application documents. */
const SCHEME = "dsh-app";
...
const applicationUrl = `${SCHEME}://app/`;
...
protocol.registerSchemesAsPrivileged([{ scheme: SCHEME, privileges: { standard: true, secure: true, ... } }]);
protocol.handle(SCHEME, (request) => {
    const url = new URL(request.url);
    if (url.hostname === "shell") return serveWebDocument(request, /* renderer/ + welcome */);
    if (url.hostname === "app")  return /* frontend assets or proxy to the host webserver */;
    return Promise.resolve(new Response(null, { status: 404 }));
});
```

Cross-check: a full-text search for `dsh://` across **all** of `C:\Users\liujj\Downloads\dsh-asar\dsh\node_modules\@deepseek-ai\`
(`*.ts, *.js, *.md, *.json, *.yaml`) returns exactly one hit — an unrelated Chrome-DevTools
placeholder in `dsh-experimental-inspector/lib/worker.js:396` (`documentURL: "dsh://host"`).
`dsh-cmdline`, `dsh-app-boot`, `dsh-desktop-host`, `dsh-api-*` and every `dsh-client-*` package
contain no deep-link parser at all. `dsh-desktop-host/lib/cli.js` declares no commander subcommands
for opening URLs.

Registry (as observed):

```
HKCU\Software\Classes\dsh\shell\open\command\(default) = "E:\Apps\DeepSeek Harness\DeepSeek Harness.exe" "%1"
```

### Accepted forms, summarised

| URL | Windows behaviour | macOS behaviour |
|---|---|---|
| `dsh://open` | starts app, or focuses the running window (single-instance) — no other effect | `focusPrimaryWindow()` |
| `dsh://open/` | same | `focusPrimaryWindow()` |
| `dsh://session/<id>` | same as `dsh://open` (id ignored) | ignored (no handler branch) |
| `dsh://open/<id>`, `dsh://new`, `dsh://workspace/<id>`, `dsh://?prompt=…` | same (all ignored) | ignored |

**Implication for a companion tool:** do not plan on deep links for "open session X". The only
reliable in-app navigation is the running web GUI itself at `http://127.0.0.1:5538` (see §5), whose
session selection is renderer state, not URL state.

---

## 2. Session deletion / archive

### Answer in one paragraph

The sidebar never deletes a session. Its session context menu / hover action offers **archive**
(`归档会话`, localised key `menu.archiveSession`) and **unarchive**, plus pin — there is no delete
verb in the client, the session controller, the workspace controller, or the JSONL persistence
backend (the only `rm()` calls in `dsh-session-persistence-jsonl` are temp/staging files and lock
files). Archive is a **flag**: the session id is appended to `global.archivedSessionIds` in
`C:\Users\liujj\.dsh\storages\workspace.json` (storage unit `{"name":"workspace","version":2}`),
and *nothing else changes* — the session log directory, the projection cache file, the workspace's
`sessionIds` accounting slot, and the on-disk log all stay exactly where they were. A disk-only
reader therefore decides "archived" solely by membership in
`<profile>/storages/workspace.json → global.archivedSessionIds` (pinned: `global.pinnedSessionIds`),
and can never observe "deleted": if a session directory disappears it was removed outside the app.
Subagent child sessions are marked in two independent, mutually consistent ways: (a) the directory
name is a **bare UUID** instead of `session-<uuid>`, because the subagent manager mints
`brandString(randomUUID())` for child ids while top-level sessions are minted
`session-${randomUUID()}`; and (b) the first line of the child's `session.v4.jsonl.zstd` is a
header carrying `"origin":"subagent"`, `"parentSession":"<parent id>"`, `"delegationDepth":1`, which
the sidebar filters on (`if (session.origin === "subagent") return false`).

### 2.1 Archive is a set in the workspace registry

`C:\Users\liujj\.dsh\storages\workspace.json` (live, plain JSON, 4511 bytes) — the fields that matter:

```json
{
  "unit": { "name": "workspace", "version": 2 },
  "global": {
    "initialized": true,
    "defaultWorkspaceId": "b6a99716-a0bf-4c62-a2d5-feb1bbcc2cf5",
    "workspaceIds": [ "090f13a7-…", "cd263945-…", "…" ],
    "archivedSessionIds": [
      "session-5a4f5419-a567-4491-9183-de01744f3de8",
      "session-cbe72ba1-612d-418c-9ce3-30d9c8e2c4f1",
      "session-28549446-3a45-4005-aebd-86fc7462066c",
      "session-0009c355-16e2-4bf0-ac57-e684a18a3806",
      "session-6c172e78-7420-4c07-a791-b1e7801b3bd0",
      "session-ffce7dc7-841b-4c3b-b64e-2cd884321fa6"
    ],
    "pinnedSessionIds": []
  },
  "tables": {
    "workspaces": {
      "<workspaceId>": {
        "path": "E:\\myAIProjects\\zetrix-dsh-screen",
        "title": "zetrix-dsh-screen",
        "sessionIds": [ "session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7" ],
        "createdAt": "2026-11-15T14:33:27.424Z",
        "updatedAt": "2026-11-15T14:33:27.698Z"
      }
    }
  }
}
```

Note that archived ids **remain inside** their workspace's `sessionIds` (e.g. `session-5a4f5419…`
is archived and still listed under `default-workspace`), and their session directories and
projection-cache files still exist. This is by design — `dsh-workspace/lib/types/index.js:257`:

```js
/**
 * The registry-global archive set: sessions hidden from every grouping
 * surface. Archiving never touches workspace accounting — an archived
 * … entry keeps its slot so unarchiving restores its position.
 */
get archivedSessionIds() { return this.requireState().archivedSessionIds; }
```

and `archiveSession()` (line 282) only writes `archivedSessionIds: [...state.archivedSessionIds, sessionId]`,
`unarchiveSession()` only filters it out (line 316). Pinning an archived session is explicitly
rejected (`WorkspaceArchivedSessionPinError`), i.e. pin and archive are mutually exclusive.

Host-side API surface (`dsh-api-workspace-controller`, `dsh-api-session-controller`) exposes
`workspace/archiveSession` and `workspace/unarchiveSession` only; `dsh-api-session-controller/lib/types/archived-session-gate.js`
makes an archived session (and its subagent descendants) refuse to run model steps. Sidebar wiring:
`dsh-client-ui-workspace/lib/client.js:1544-1550` (`archiveSession` / `unarchiveSession` →
`workspaces.archiveSession(...)`), menu registration at `:4382`, row button at `:4172`,
stop-and-archive confirmation at `:5879`.

Also relevant: `deleteWorkspace` deletes only the *workspace registration* — "Delete one workspace
registration while retaining its directory and every **session**" (`dsh-workspace/lib/types/index.js:220`).

### 2.2 Nothing on disk distinguishes "deleted"

* `dsh-session-persistence-jsonl` has no delete/purge operation; `rm()` is used for
  `.dsh-mkdir-*` staging, lease files, `session.migration.*.tmp`, and cache temp files only.
* `dsh-session-query*`, `dsh-workspace`, `dsh-session-projection-cache` contain no
  `deleteSession|removeSession|purgeSession`.
* The sidebar's only row-level destructive verbs are archive/unarchive, pin/unpin, rename, fork.

So a disk-only reader should model lifecycle as:
`live dir present` + `archived ∈ global.archivedSessionIds` + `blank ∈ projection.sessionListMetadata.blank`
and treat a missing directory as "removed out of band" (no flag to read).

### 2.3 Session directory layout and naming

Paths (`dsh-session-persistence-jsonl/lib/index.js`, `encodeSegment` / `projectKey` / `sessionDir` /
`generationLogPath`, lines ~860-935):

```js
function projectKey(cwd) {                       // "--E-myAIProjects-zetrix-dsh-screen--"
  // '/' '\\' and ':' collapse to '-', unsafe code units become ~XXXX, wrapped in "--…--", max 251 chars
  return `--${(readable.replace(/^-+/, "") || "root").slice(0, 251)}--`;
}
function projectDir(root, cwd) { return cwd === void 0 ? join(root, "_no-cwd") : join(root, projectKey(cwd)); }
function sessionDir(root, cwd, id) { return join(projectDir(root, cwd), encodeSegment(id)); }
function generationLogPath(root, cwd, id, version, compression) { return join(sessionDir(root, cwd, id), generationLogFilename(version, compression)); }
```

Live example:

```
C:\Users\liujj\.dsh\sessions\--E-myAIProjects-zetrix-dsh-screen--\
    session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7\session.v4.jsonl.zstd   (772 222 B)  ← top-level
    0f8d380d-cd24-4cfd-8fba-603680aafa33\session.v4.jsonl.zstd          (346 171 B)  ← subagent child
    26be873d-f796-4795-b02e-d57e9d47591e\session.v4.jsonl.zstd          (196 700 B)  ← subagent child
```

Each session directory currently contains exactly one file, the zstd-compressed JSONL generation
`session.v4.jsonl.zstd` (format version 4, zstd compression). The directory name is the session id,
path-escaped by `encodeSegment` (only `~XXXX` escapes; UUIDs and the `session-` prefix are untouched).

### 2.4 Telling a subagent child from a top-level session

**(a) Directory name.** `session-<uuid>` ⇒ minted by a top-level creator
(`dsh-api-session-controller/lib/types/agent.js:454`, `dsh-headless/lib/index.js:333`,
`dsh-schedule`, `dsh-agent-loop/lib/index.js:1350`). Bare `<uuid>` ⇒ minted as a subagent child id:

```js
// dsh-subagent/lib/types/manager.js:322  (same logic in dsh-subagent/lib/index.js:1403)
const childId = spec.childId ?? brandString(randomUUID());
```

**(b) Session header (first JSONL line, authoritative).** Decompressing the first zstd frame gives:

```json
// child:  sessions\--E-myAIProjects-zetrix-dsh-screen--\0f8d380d-…\session.v4.jsonl.zstd
{"type":"session","version":4,"id":"0f8d380d-cd24-4cfd-8fba-603680aafa33",
 "createdAt":1794753678161,"cwd":"E:\\myAIProjects\\zetrix-dsh-screen",
 "parentSession":"session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7",
 "isSeeded":false,"origin":"subagent","delegationDepth":1,"agentPreset":"standard"}

// top-level: sessions\--E-myAIProjects-zetrix-dsh-screen--\session-fa302a65-…\session.v4.jsonl.zstd
{"type":"session","version":4,"id":"session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7",
 "createdAt":1794753207647,"cwd":"E:\\myAIProjects\\zetrix-dsh-screen",
 "isSeeded":false,"delegationDepth":0,"agentPreset":"standard"}
```

**(c) The host's own list payload and the sidebar filter.** `dsh-api-session-controller/lib/types/list.js`
propagates the header verbatim into every row:

```js
function listFields(header) {
    return {
        ...(header.parentSession === undefined ? {} : { parentSessionId: header.parentSession }),
        ...(header.origin === undefined ? {} : { origin: header.origin }),
        ...(header.cwd === undefined ? {} : { cwd: header.cwd }),
    };
}
```

```js
// dsh-client-ui-workspace/lib/client.js:964
function sessionVisible(session, current, archived, archivedFilter) {
    if (session.origin === "subagent") return false;          // children never get a sidebar row
    if (session.blank && session.id !== current) return false; // blank sessions hidden except current
    switch (archivedFilter) { case "default": return !archived.has(session.id); ... }
}
```

So: **`origin === "subagent"` (or `parentSession` present, or `delegationDepth > 0`, or a bare-UUID
directory) ⇒ child session, not a sidebar row.** The parent's own projection row `subagentCatalog`
additionally carries the child list (see §3), which is how the sidebar shows
`{n} 个子智能体运行中`.

---

## 3. Sidebar status and title sources

### Answer in one paragraph

Titles, blankness, recency, turn/step statistics, context pressure, pending *questions* and the
turn/step boundary state are durable and readable from
`C:\Users\liujj\.dsh\storages\session_projcache\sessions\<sessionId>.json` — **plain UTF-8 JSON**
(no compression), envelope `{"version":7,"record":{"identity":{…},"rows":{<key>:{ver,seq,val}}}}`,
written by `dsh-session-projection-cache` into the JSON storage domain
`<dsh-home>/storages/session_projcache/sessions/<id>.json`. The row `title` is the sidebar title,
`sessionListMetadata` gives `blank` and `lastPromptAt` (recency: `updatedAt = max(header.createdAt,
lastPromptAt)`), `sessionStats` gives turns/steps/speed, `turnBoundary` gives
`openTurnStartSeq`/`lastStepBoundary` (a reliable "a turn/step is still open" signal on disk),
`approvalState` gives only `{policy, turnOpen}`, and `userQuestions.questions.active` is the durable
"waiting for an answer" state. What is **not** durable is the live *running* flag, the
"pending approval" interaction, and the "已完成" (completed-unread) badge: those three are computed
in the renderer from live session events (`api-session/status`, the approval/question UI queues,
`running→false` transitions) and never persisted. The sidebar labels themselves are i18n strings in
`dsh-client-ui-workspace/lib/client.js` (`status.running` = 进行中, `status.idle` = 空闲,
`status.waitingApproval` = 等待审批, `status.planReview` = 计划待审, `status.waitingAnswer` =
等待回答, `status.completed` = 已完成, `status.subagentsRunning.*` = {n} 个子智能体运行中).

### 3.1 Projection cache: path, format, identity

`dsh-session-projection-cache/README.zh.md` states the contract precisely:

> 会话日志始终是权威：崩溃可能使检查点陈旧，但不会使其领先于已提交事件 …
> JSON 后端把每条记录存于仅所有者可访问的 `<root>/session_projcache/sessions/<id>.json` 目录树中。

where `<root>` is `dshHomePath('storages')` = `C:\Users\liujj\.dsh\storages`. Envelope observed for
the live session `session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7`:

```json
{
  "version": 7,
  "record": {
    "identity": { "formatVersion": 4, "createdAt": 1794753207647,
                  "cwd": "E:\\myAIProjects\\zetrix-dsh-screen",
                  "isSeeded": false, "inheritedEventCount": 0 },
    "rows": {
      "title":               { "ver": 1, "seq": 736, "val": "将 zectrix-workbuddy-pet 从 workbuddy 改为接入 deepseek harness" },
      "sessionListMetadata": { "ver": 1, "seq": 736, "val": { "blank": false, "lastPromptAt": 1794754031549 } },
      "sessionStats":        { "ver": 1, "seq": 736, "val": { "turns": 2, "steps": 74, "llmMs": 440860, "toolMs": 362532,
                                 "ttftMs": 77665, "ttftSteps": 74, "decodeMs": 363195, "decodeTokens": 76798,
                                 "lastTurn": 2, "openStep": null, "pendingCalls": {} } },
      "turnBoundary":        { "ver": 2, "seq": 736, "val": { "openTurnStartSeq": null, "lastStepStartSeq": 730,
                                 "lastStepBoundary": { "kind": "end", "seq": 734 }, "lastTurn": 2 } },
      "approvalState":       { "ver": 1, "seq": 736, "val": { "policy": "never", "turnOpen": false } },
      "userQuestions":       { "ver": 2, "seq": 736, "val": { "inheritedEventCount": 0, "timed": false,
                                 "questions": { "active": [], "settled": [] } } },
      "contextPressure":     { "ver": 5, "seq": 736, "val": { "surfaceTokens": 163885, "contextWindow": 1048576,
                                 "pressureTokens": 218676, "sampledSurfaceTokens": 162619 } },
      "permissions":         { "ver": 2, … "val": { "preset": "danger-full-access", … } },
      "goal": { … }, "plan": { … }, "todos": { … }, "modelSelection": { … }, "workingDirectory": { … },
      "subagentCatalog": { … }, "subagent": { … }, "titleInput": { … }, "inbox": { … }, "contextBreakdown": { … }
    }
  }
}
```

Row keys seen across all 31 cached sessions in this profile:
`agentPreset, agentTeam, approvalState, codingns*Stats, contextBreakdown, contextPressure, goal,
harnessDevLegacyPrompt, imageLimits, inbox, llmRetry, modelSelection, permissions, plan, sandboxMode,
sessionListMetadata, sessionStats, subagent, subagentCatalog, subagentModelSelectionPolicy,
subagentTiming, timeContext, title, titleInput, todos, tokenMeasurement, tokenUsage, turnBoundary,
turnOutline, userQuestions, workingDirectory`.

The same file is written for archived sessions and for subagent children (children get their own
`<bare-uuid>.json`), so the cache is not a "visible sessions" index — it is one checkpoint per
session that ever ran.

### 3.2 Which rows a disk-only reader needs

| Need | File / field | Notes |
|---|---|---|
| Sidebar title | projcache `record.rows.title.val` (string) | `sessionTitle()` returns `""` when `blank`; the renderer localises the placeholder (`session.new`="新会话", `session.untitled`="未命名") |
| Blank / new-session row | `record.rows.sessionListMetadata.val.blank` | `applySessionListMetadata`: `blank` starts `true`, flips `false` on the first `turn/start`; a blank session is shown only while it is the current one |
| Recency / "updatedAt" | `sessionListMetadata.val.lastPromptAt` + header `createdAt` | `updatedAt = Math.max(header.createdAt, metadata?.lastPromptAt ?? 0)` (`dsh-api-session-controller/lib/types/list.js:334`) |
| Live "running" (best effort) | `turnBoundary.val.openTurnStartSeq !== null` **or** `approvalState.val.turnOpen === true` **or** `turnBoundary.val.lastStepBoundary.kind === "start"` **or** non-empty `sessionStats.val.pendingCalls` | These are projections of committed `turn/start`,`turn/end`,`step/start`,`step/end` events, so they are correct as of the last checkpoint write (observed lag ~seconds; cache writes are throttled and also forced at `turn/end` and session dispose) |
| Turn/step counters, tok/s | `sessionStats.val.{turns,steps,decodeTokens,decodeMs}` | tok/s = `decodeTokens / (decodeMs/1000)` — exactly what the UI pill prints |
| Waiting for answer | `userQuestions.val.questions.active` (non-empty) | durable; projection `key:'userQuestions'`, `stateVersion:2`, folds `tool/call`/`tool/result` of `ask_user_question` |
| Waiting for approval | **not durable as a flag** | `approvalState` only has `{policy:'ask'\|'never', turnOpen}`; the pending ask exists only in memory, though the log records `approval/asked` / `approval/decided` pairs you could pair up yourself |
| Plan review pending | `plan` row + live plan-mode state | not a durable boolean in the cache |
| Context used % | `contextPressure.val.pressureTokens / contextWindow` | UI uses `projectedTokens ?? pressureTokens`; `Math.min(100, Math.round(used/contextWindow*100))` |
| Archived | `storages/workspace.json → global.archivedSessionIds` | not in projcache |
| Child session | log header `origin:"subagent"` / `parentSession` | not in projcache |

### 3.3 How the UI turns that into a status label

`dsh-client-ui-workspace/lib/client.js` — priority: pending interaction > running > subagents >
completed > idle:

```js
function sessionStatuses(node, t) {
    const subagents = node.runningSubagentCount === 0 ? void 0
        : { state: "ongoing", label: t(node.runningSubagentCount === 1 ? "status.subagentsRunning.one" : "status.subagentsRunning.other", { n: node.runningSubagentCount }) };
    let pending;
    switch (node.pendingInteraction) {
        case "approval":    pending = { state: "warning", label: t("status.waitingApproval"), trailingLabel: t("status.compact.approval") }; break;
        case "plan-review": pending = { state: "warning", label: t("status.planReview"),    trailingLabel: t("status.compact.planReview") }; break;
        case "question":    pending = { state: "warning", label: t("status.waitingAnswer"), trailingLabel: t("status.compact.answer") }; break;
        case void 0: break;
        default: return assertNever(node.pendingInteraction);
    }
    if (pending !== void 0) return subagents === void 0 ? [pending] : [pending, subagents];
    if (node.running) { const primary = { state: "ongoing", label: t("status.running") }; return subagents === void 0 ? [primary] : [primary, subagents]; }
    if (subagents !== void 0) return [subagents];
    if (node.completed) return [{ state: "done", label: t("status.completed") }];
    return [{ state: "idle", label: t("status.idle") }];
}
```

Field origins for that node (`sessionNode()`, line ~1055): `running: status?.running ?? s.running`,
`completed: status?.completionUnread === true`,
`pendingInteraction: visiblePendingKind(status?.pendingInteraction?.kind)`.
The status store is renderer-side and in-memory (`dsh-client-ui-session/lib/client.js:774-1003`):

```js
running = new Map(); completionUnread = new Set();
const disposeRemoteStatus = ctx.remote.$on("api-session/status", (sessionId, running) => { this.observeRunning(sessionId, running); });
...
observeRunning(sessionId, running) {
    const previous = this.running.get(sessionId);
    this.running.set(sessionId, running);
    if (running) this.completionUnread.delete(sessionId);
    else if ((previous === true || (previous === void 0 && beforeBaseline)) && !this.isMain(sessionId)) this.completionUnread.add(sessionId);
}
```

`api-session/status` is emitted by the host from agent lifecycle
(`dsh-api-session-controller/lib/types/index.js:319`: `ctx.emit('api-session/status', agent.id, status === 'running')`,
driven by the `agent/status` event). Pending-interaction kinds are registered by the feature UIs through
`uiSession.registerPendingInteraction(precedence)`: `"approval"` from `dsh-client-ui-approval`, and
**both** `"question"` and `"plan-review"` from `dsh-client-ui-user-questions` —
`this.kind = planReviewOf(questions) === void 0 ? "question" : "plan-review"`
(`dsh-client-ui-user-questions/lib/client.js:185`); the plan UI only renders the takeover
(`dsh-client-ui-plan/lib/client.js:792`, slot `conversation.plan-review.actions`).

Consequences for the display tool:

* `进行中` (running) and `已完成` (completed-unread) are **live UI facts**, not on-disk flags. The
  closest disk proxy for `进行中` is §3.2's turnBoundary/approvalState heuristic. There is no disk
  proxy for `已完成` — it means "finished while the user was not looking at that session".
* `等待回答` is a real durable state (`userQuestions.questions.active`).
* `等待审批` / `计划待审` are live-only unless you replay the log tail for
  `approval/asked` without `approval/decided`, or plan-mode events.
* `空闲` is simply "not running, not completed-unread, no pending interaction".

### 3.4 What drives the durable rows (event types)

| Row | Projection owner | Events |
|---|---|---|
| `turnBoundary` | `dsh-agent/lib/types/projection.js` (`key:'turnBoundary'`, `stateVersion:2`) | `turn/start` → `openTurnStartSeq = seq`; `turn/end` → `null`; `step/start`/`step/end` → `lastStepBoundary: {kind:'start'\|'end', seq}` |
| `approvalState` | `dsh-user-approval/lib/types/index.js:23-46` | `approval/policy`, `turn/start` (`turnOpen:true`), `turn/end` (`turnOpen:false`) |
| `userQuestions` | `dsh-user-questions/lib/types/projection.js:281` | `tool/call` + `tool/result` for `ask_user_question` (open/continued/settled) |
| `sessionListMetadata` | `dsh-api-session-controller/lib/types/list.js:105` | `blank` cleared by first `turn/start`; `lastPromptAt` = time of a `user/message` whose `data.source.kind === 'user'` |
| `sessionStats` | `dsh-session-stats/lib/types/projection.js:67` (`key:'sessionStats'`) | step/llm/tool timings; `pendingCalls` while a tool call is in flight |
| `contextPressure` | token meter | `surfaceTokens`, `contextWindow`, `pressureTokens`, `sampledSurfaceTokens` |

---

## 4. Live status text for a session (for a small desktop display)

### Answer in one paragraph

The strings you see in UI Automation come from two different renderers.
**"正在运行命令 · <description>"** is the live step-process group header in
`@deepseek-ai/dsh-client-ui-chat`: the label is `t("message.stepProcess." + activity)` where
`activity` is a categorised tool kind (`commands` for `bash/pwsh/exec_command/write_stdin/terminal_*`,
also `read, readImage, write, edit, search, code, webSearch, webFetch, subagents, plan, questions,
tools, thinking`) and the detail is a one-line excerpt of the running tool's arguments (keys:
`title, description, objective, task, task_name, name, question(s), prompt, message, command, cmd,
queries, query, pattern, url, …`), joined by the locale separator `" · "`
(`message.turnProcess.separator`). While the tool call is still being prepared the label switches to
`message.stepProcess.prepare.<kind>` ("准备运行命令"). **"深度求索中"** is a separate element at the
end of the transcript (`RunningStatus`, `data-chat-running`) shown while the session is running;
it becomes `深度求索中，用时 {duration} ···` once a turn start time is known. **"1 轮 39 步 · 206 tok/s"**
is a composer stat pill: `t("stats.counts", {turns, steps})` + `" · "` +
`t("message.tokensPerSecond")`, fed by the `sessionStats` projection. **"上下文已用 12%"** is the
`ContextMeter` in `dsh-client-ui-conversation`: `t("context.aria", {percent})` where percent comes
from the `contextPressure` projection. All four values are derivable from session events / the
projection cache instead of UIA: the tool activity + description from the in-flight `tool/call`
event, the turn/step/speed numbers from `sessionStats`, and the context percentage from
`contextPressure` (the very same rows the UI reads).

### 4.1 i18n keys and locations

**`C:\Users\liujj\Downloads\dsh-asar\dsh\node_modules\@deepseek-ai\dsh-client-ui-chat\lib\client.js`**
(zh dictionary is the key-set source of truth, region `lib/types/client/locale.js`):

```js
"message.stepProcess.thinking": "正在分析请求",
"message.stepProcess.read": "正在读取文件",
"message.stepProcess.readImage": "正在读取图片",
"message.stepProcess.write": "正在写入文件",
"message.stepProcess.search": "正在搜索代码",
"message.stepProcess.edit": "正在编辑文件",
"message.stepProcess.commands": "正在运行命令",        // line 6542
"message.stepProcess.code": "正在运行代码",
"message.stepProcess.webSearch": "正在搜索网页",
"message.stepProcess.webFetch": "正在访问网页",
"message.stepProcess.subagents": "正在协调子智能体",
"message.stepProcess.plan": "正在更新计划",
"message.stepProcess.questions": "等待你的操作",
"message.stepProcess.tools": "正在调用工具",
"message.stepProcess.prepare.commands": "准备运行命令", // + prepare.* for every kind
"message.stepProcess.done.commands": "执行了命令",      // + done.* for every kind
"stats.counts": "{turns} 轮 {steps} 步",               // line 6605
"message.tokensPerSecond": "{tps} tok/s",              // line 6708
"chat.deepDiving": "深度求索中",                        // line 6617
"chat.deepDivingFor": "深度求索中，用时 {duration} ···", // line 6618
"message.turnProcess.separator": " · ",                // line 6688
```

**`…\@deepseek-ai\dsh-client-ui-workspace\lib\client.js`** (sidebar rows):

```js
"status.running": "进行中",            // 4857
"status.subagentsRunning.one": "{n} 个子智能体运行中",
"status.idle": "空闲",                 // 4860
"status.waitingApproval": "等待审批",   // 4861
"status.planReview": "计划待审",        // 4862
"status.waitingAnswer": "等待回答",     // 4863
"status.compact.approval": "待审批", "status.compact.planReview": "计划待审", "status.compact.answer": "待回答",
"status.completed": "已完成",           // 4867
"menu.archiveSession": "归档会话", "menu.unarchiveSession": "取消归档",
```

**`…\@deepseek-ai\dsh-client-ui-conversation\lib\client.js`**:

```js
"context.aria": "上下文已用 {percent}",  // 16475
"context.used": "上下文已用",            // 16476
```

### 4.2 How the composed strings are produced

Tool-activity label + detail (`dsh-client-ui-chat/lib/client.js`):

```js
// process-activity.js — tool name ⇒ activity category
function activity(name) {
    if (name === "read") return "read";
    if (name === "read_image") return "readImage";
    if (name === "grep" || name === "glob" || name.endsWith("_inspect")) return "search";
    if (name === "write") return "write";
    if (name === "edit" || name === "apply_patch") return "edit";
    if (["bash", "pwsh", "exec_command", "write_stdin"].includes(name) || name.startsWith("terminal_")) return "commands";
    if (name === "run_code") return "code";
    ...
}
// ChatGroupSeat.js — header title
const label  = data.closed ? processTitle(data.summary, t)
             : live.preparing ? t(`message.stepProcess.prepare.${live.activity === "thinking" ? "tools" : live.activity}`)
             : t(`message.stepProcess.${live.activity}`);
const detail = detailed && !data.closed ? live.detail : "";
const title  = detail === "" ? label : `${label}${t("message.turnProcess.separator")}${detail}`;
```

So the exact pattern is `"<正在…>"` or `"<正在…> · <detail>"` (detail is only appended when the
"live process detail" presentation policy is on; `<160` graphemes, from the argument keys listed
above). `processActivity(nodes)` picks the **latest running tool** (`if (isRunningTool(tool) &&
tool.time >= runningTime)`) and marks `preparing` for `tool.phase === "preparing"`.

Running indicator (`RunningStatus`, same file around line 6402):

```js
const label = startTime === void 0 ? t("chat.deepDiving")
            : t("chat.deepDivingFor", { duration: formatRunDuration(Math.max(1e3, now - startTime), t)… });
```

Stats pill (`ActivityPill`, same file ~9650):

```js
const stats  = useSessionStats(useChat, useProjection);          // projection row sessionStats ?? deriveStats(nodes)
const speed  = stats.decodeMs > 0 ? decodeSpeed(stats, t) : null; // t("message.tokensPerSecond", {tps: formatTokensPerSecond(decodeTokens/(decodeMs/1e3))})
const counts = t("stats.counts", { turns: stats.turns, steps: stats.steps });
// pill text = `${counts} · ${speed}`  →  "1 轮 39 步 · 206 tok/s"
```

Context meter (`ContextMeter`, `dsh-client-ui-conversation/lib/client.js:21440`):

```js
function contextOccupancy(pressure) {
    const usedTokens = pressure?.projectedTokens ?? pressure?.pressureTokens;
    if (usedTokens === void 0 || pressure?.contextWindow === void 0) return null;
    return { percent: Math.min(100, Math.round(usedTokens / pressure.contextWindow * 100)), usedTokens, contextWindow: pressure.contextWindow };
}
const reading = `${percent}%`;                                  // rendered next to the ring
const [headBefore, headAfter] = t("context.aria", { percent: READING_SLOT }).split(READING_SLOT); // "上下文已用" / ""
```

### 4.3 Which string appears when

| Situation | UI element | Text pattern |
|---|---|---|
| Turn running, no tool in flight (model streaming / reasoning) | `RunningStatus` at transcript tail | `深度求索中` (or `深度求索中，用时 12秒 ···`) |
| Turn running, a tool call `preparing` (arguments streaming) | step-process group header | `准备运行命令`, `准备读取文件`, … (`prepare.<activity>`) |
| Turn running, tool call executing | step-process group header | `正在运行命令 · <description>` (`<activity>` + `" · "` + detail) |
| Turn finished | group header, closed | `已完成`, `已读取文件`, `执行了命令`, … (`done.<activity>`, `processTitle`) |
| Any state, composer dock | `ActivityPill` | `{turns} 轮 {steps} 步` · `{tps} tok/s` (compact mode keeps only tok/s) |
| Any state with a reported context window | `ContextMeter` | ring + `12%`, tooltip/aria `上下文已用 12%` |
| Sidebar row | `SessionStatusDots` (visually hidden label) + dot state | `进行中` / `等待审批` / `计划待审` / `等待回答` / `已完成` / `空闲` / `{n} 个子智能体运行中` |

### 4.4 Derivable from session events instead of UI Automation?

Yes for everything except the "completed-unread" badge and the exact wording locale:

* Tool activity + description ⇒ last open `tool/call` event in the session log (or live stream):
  `dsh-tools/lib/pipeline.js:188` appends `session.append("tool/call", { turn, step, callId, name,
  arguments })`, so the log carries the tool name and its argument text; `name` maps to the activity
  category via `activity()`, and `arguments` supplies the detail in `LIVE_TOOL_DETAIL_KEYS` order.
  A reader can therefore reproduce `正在运行命令 · <description>` deterministically.
* `深度求索中` ⇒ "session running and no tool running"; running is `agent/status` on the host
  (live only), disk proxy = `turnBoundary.openTurnStartSeq !== null`.
* `N 轮 M 步 · X tok/s` ⇒ projcache `sessionStats` row (identical numbers; the UI reads the same row).
* `上下文已用 P%` ⇒ projcache `contextPressure` row, same formula as `contextOccupancy`.
* Sidebar statuses ⇒ §3.3 mapping; `已完成` has no durable equivalent.

If the companion tool wants *exact* UI strings, prefer reading them from UIA; if it wants stable
semantics, read the projection cache + log tail (locale-independent).

---

## 5. Processes and window

### Answer in one paragraph

Every OS process belonging to the desktop app is literally named **`DeepSeek Harness`**
(image `E:\Apps\DeepSeek Harness\DeepSeek Harness.exe`): the browser main process, the GPU process,
the network utility, the renderer, the **host process that actually owns all sessions**
(`...\dsh-desktop-host\lib\index.js`, started by the main process with the same exe), and the
per-tool subprocess runners (`dsh-subprocess-local\lib\runner.js`, e.g. one per running `pwsh` tool).
None of them is `node.exe`, and no separate `dsh` daemon hosts the GUI's sessions — so a
process-name check on `DeepSeek Harness` is the correct "app running" test. The only other way DSH
runs is the headless/CLI entry `runtime\cli\bin\dsh.cmd`, which also execs the *same*
`DeepSeek Harness.exe` (with `ELECTRON_RUN_AS_NODE=1`) and would therefore also appear under the same
process name while running a separate, independent session tree; `node.exe` processes on this machine
belong to unrelated tooling (Codex runtimes), not to DSH.

### Evidence — observed processes

```
Id     PPID   Name                   CommandLine (trimmed)
35304  14996  DeepSeek Harness.exe   "E:\Apps\DeepSeek Harness\DeepSeek Harness.exe"
37236  35304  DeepSeek Harness.exe   … --type=gpu-process --user-data-dir="C:\Users\liujj\AppData\Roaming\@deepseek-ai/dsh-desktop" …
26752  35304  DeepSeek Harness.exe   … --type=utility --utility-sub-type=network.mojom.NetworkService --lang=zh-CN …
23484  35304  DeepSeek Harness.exe   … --type=renderer --standard-schemes=dsh-app --secure-schemes=dsh-app …
21244  35304  DeepSeek Harness.exe   "…\DeepSeek Harness.exe" --expose-internals "…\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js" …
10620  21244  DeepSeek Harness.exe   "…\DeepSeek Harness.exe" "…\dsh-subprocess-local\lib\runner.js" -- "C:\Program Files\PowerShell\7\pwsh.exe" …
4952   21244  DeepSeek Harness.exe   "…\DeepSeek Harness.exe" "…\dsh-subprocess-local\lib\runner.js" -- "C:\Program Files\PowerShell\7\pwsh.exe" …
```

* PID 35304 = Electron main (holds the single-instance lock, the `dsh://` registration, the window).
* PID 21244 = the Node-mode **desktop host** — it hosts the profile, the HTTP/WebSocket server and
  all session agents (`dsh-desktop-host`'s package description: "Private Node-mode host process for
  the Electron desktop application"). It is the same executable, **not** `node.exe`.
* The renderer (PID 23484) is where the sidebar/conversation status strings live; its document
  origin is `dsh-app://app/`.
* Web GUI URL observed in this session: `http://127.0.0.1:5538` (the host webserver the renderer
  proxies to).

### The other DSH entry point

`E:\Apps\DeepSeek Harness\resources\runtime\cli\bin\dsh.cmd`:

```bat
set "ELECTRON_RUN_AS_NODE=1"
"%~dp0..\..\..\..\DeepSeek Harness.exe" --expose-internals "%~dp0..\..\..\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\cli.js" %*
```

So a terminal `dsh` run is also a process named `DeepSeek Harness`, running `dsh-desktop-host/lib/cli.js`
in Node mode. It shares `C:\Users\liujj\.dsh` (sessions/storages) but is an independent process with
its own session tree. `dsh` is not currently on `PATH` in this shell (`Get-Command dsh` → nothing),
and no `node.exe` process belongs to DSH.

**Practical rule for the pet:** `app running ⇔ any process whose ProcessName is "DeepSeek Harness"`
(optionally require the main process to have a `--type=`-less command line, or the window title).
Do not check for `node`, `dsh`, `electron`, or `dsh-desktop-host` in the image path — none of those
appear as a process name for the desktop app.

---

## Appendix A — reading `lib/main.js` out of the asar without extracting it

```powershell
$p = "E:\Apps\DeepSeek Harness\resources\app.asar"
$fs = [IO.File]::OpenRead($p); $br = New-Object IO.BinaryReader($fs)
$null=$br.ReadUInt32(); $null=$br.ReadUInt32(); $null=$br.ReadUInt32()
$jsonLen = $br.ReadUInt32()                       # 4th uint32 is the JSON index length
$hdrLen  = 16 + $jsonLen
$asar    = ([Text.Encoding]::UTF8.GetString($br.ReadBytes($jsonLen)) | ConvertFrom-Json)
$entry   = $asar.files.lib.files.'main.js'        # size 838520, offset relative to $hdrLen
$fs.Position = $hdrLen + [int64]$entry.offset
$text    = [Text.Encoding]::UTF8.GetString($br.ReadBytes([int]$entry.size))
$br.Close(); $fs.Close()
$text.IndexOf('setAsDefaultProtocolClient')       # 814941
```

## Appendix B — reading a session header / decompressing a log (bundled Node has zstd)

```powershell
$node = "E:\Apps\DeepSeek Harness\resources\runtime\primary-runtime\dependencies\node\bin\node.exe"
& $node -e "const z=require('zlib'),fs=require('fs');const t=z.zstdDecompressSync(fs.readFileSync(process.argv[1])).toString('utf8');console.log(t.split('\n')[0])" `
  "C:\Users\liujj\.dsh\sessions\--E-myAIProjects-zetrix-dsh-screen--\0f8d380d-cd24-4cfd-8fba-603680aafa33\session.v4.jsonl.zstd"
```

(Note: `zstdDecompressSync` returns only the **first frame** of the multi-frame log — enough for the
header. The system `node` v22.14 in `C:\Program Files\nodejs` has no `zlib.zstd*`.)

## Appendix C — quick disk-only status probe (validated against the live session)

For the session that was running while this research was done
(`session-fa302a65-…`, file refreshed 22:56:34), the projcache contained:

```json
"turnBoundary":  {"ver":2,"seq":1100,"val":{"openTurnStartSeq":738,"lastStepStartSeq":1096,
                  "lastStepBoundary":{"kind":"start","seq":1096},"lastTurn":3}}
"sessionStats":  {"ver":1,"seq":1100,"val":{"turns":3,"steps":118,…,"lastTurn":3,"openStep":null,
                  "pendingCalls":{"call_00_k0rfKvSlag45eZB6hNi99846":1794754592198}}}
"approvalState": {"ver":1,"seq":1100,"val":{"policy":"never","turnOpen":true}}
```

i.e. `openTurnStartSeq != null && turnOpen == true && lastStepBoundary.kind == "start" &&
pendingCalls != {}` ⇔ "a turn and a step are currently open, a tool call is in flight", and earlier
the same file read `openTurnStartSeq: null, turnOpen: false, lastStepBoundary.kind: "end"` while the
session was between turns. That is the most reliable durable "working" signal available.

## What could not be determined / caveats

1. **No session deep link exists in the shipped code.** This is a negative result from reading the
   full main-process bundle and grepping every package; it is not a runtime experiment. If a
   companion tool must open a specific session, it should target the running web GUI
   (`http://127.0.0.1:<port>`, port 5538 in this session) or the WebSocket/API gateway, not `dsh://`.
2. **Pending approval is not persisted as a flag.** `approvalState` only records policy + turn-open.
   Reconstructing "waiting for approval" from disk requires pairing `approval/asked` with
   `approval/decided` in the JSONL log tail (event names confirmed in
   `dsh-user-approval/lib/types/index.js:145,152`), which this report did not implement/validate.
3. **"已完成" (completed) has no durable representation**; it is the renderer's
   `completionUnread` set (`running` transitioned to false while the session was not the open one).
4. **Projection-cache write cadence** (`writeEveryEvents`, `writeIntervalMs`) is configured by the
   desktop composition, which is not shipped in readable form here; the README says both are
   required, and observed staleness was a few seconds (the cache is also force-written at `turn/end`
   and at session dispose). Treat `seq` in each row as the watermark.
5. **Multi-frame zstd logs**: only the first frame (the header line) was decoded here; full-log
   replay would need a loop over frames or `zstd` CLI.
