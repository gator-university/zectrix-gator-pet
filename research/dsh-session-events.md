# DSH Session Log Format — Authoritative Event & State Spec

Target: **DeepSeek Harness desktop v0.2.2-rc.1**, Windows, Session format **v4**.
Audience: a Rust daemon that reads session logs **read-only, metadata only**, to drive an e-ink status display.

Sources used (both read-only):

- Extracted app source: `C:\Users\liujj\Downloads\dsh-asar\dsh\node_modules\@deepseek-ai\` (all `path:line` citations below are relative to that directory).
- Real logs: `C:\Users\liujj\.dsh\sessions\<project-slug>\<session-dir>\session.v4.jsonl.zstd` — 31 files, 37.8 MB uncompressed total, largest 4.22 MB / 856 events (compressed 1.24 MB).
- Derived state: `C:\Users\liujj\.dsh\storages\session_projcache\sessions\<sessionId>.json` and `C:\Users\liujj\.dsh\storages\workspace.json`.

Every JSON shape below was cross-checked against the compiled TypeScript declarations that the app itself ships
(`*/lib/typert.host.js` embeds the full `SessionEventMap` declaration strings) **and** against real log rows.
Optionality is taken from the declaration (`?`), not from what happened to appear in this corpus.

> **TL;DR for the implementer**
> 1. Tail the `.jsonl.zstd` **frame by frame** (each frame is an independent, checksummed zstd frame; frame 0 is the header only).
> 2. Fold events in `seq` order into a tiny state machine: `openTurn`, `openStep`, `pendingCalls`, `pendingApprovals`.
> 3. Map that state to `working / waiting-question / waiting-approval / done / stopped / failed` (Section 5).
> 4. Optionally read the projection cache for title/todos/context, but re-check it against the log watermark (Section 7).

---

## 0. Conventions

- `seq`: `number`, 0-based, assigned to **every** event after the header, contiguous (`seq == line_index - 1`), never reused. Verified on all 31 logs.
- `time`: `number`, **epoch milliseconds** (`Date.now()`), non-decreasing in every log (verified).
- `data`: object, always present on events (never on the header line).
- `ignorable?: true`: only on out-of-tree plugin events with a type outside `KNOWN_SESSION_EVENT_TYPES`; absent in this corpus.
- `surfaceOp`: present on the events that mutate the model-visible transcript (`user/message`, `system/message`, `developer/message`, `tool/result`). Value is either the string `"append"` or an object `{op:"replace", startSeq:number, endSeq:number}`. 2932 `append` + 1 `replace` observed.
- `sourceEventSeqs`: optional `number[]` on `user/message`, `system/message`, `tool/result`, `developer/message`. **Storage form**: consecutive runs of ≥3 numbers are collapsed to `[start,end]` pairs (`[[10,14],22,24,...]`). Readers must expand.
- Message content blocks (`ContentBlockMap`): `text {type:"text", text}`, `reasoning {type:"reasoning", text}`, `image {type:"image", attachment, offloaded?}`, `file {type:"file", attachment}`, `tool-call {type:"tool-call", id, name, arguments:string}`, `tool-addition {type:"tool-addition", toolName}`, `tool-removal {type:"tool-removal", toolName}`.

`seq`/`time`/`data`/`type` are the only envelope fields the reader must require; a record whose `type` it does not know must be skipped, not treated as corruption.

---

## 1. Files on disk, identity, and growth

### 1.1 Exact contents of a session directory

```
C:\Users\liujj\.dsh\sessions\--E-myAIProjects-zetrix-dsh-screen--\
   session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7\
      session.v4.jsonl.zstd          <-- the ONLY file in the directory
```

Measured over all 31 session directories (recursive, including hidden files): **exactly one file per directory**, the compressed log. There is **no** `meta.json`, `session.json`, `.lock`, heartbeat, or index file inside a session directory in this build.

Locking (relevant only if you ever write): `dsh-session-persistence-jsonl/README.md:168` — POSIX takes `flock(2)` on `session.lock`; **on Windows it is a named kernel semaphore derived from that path, "with no filesystem footprint"**. So on Windows there is no lock file to observe.

Other files that *are* worth reading (outside `sessions\`):

| Path | What it is |
|---|---|
| `~/.dsh/storages/workspace.json` | Workspace registry: `workspaces[].path/title/sessionIds`, `global.archivedSessionIds`, `pinnedSessionIds`, `defaultWorkspaceId`. JSON, whole-file atomic replace. |
| `~/.dsh/storages/session_projcache/sessions/<sessionId>.json` | Persisted projection checkpoint per session (title, todos, turn boundary, context pressure, …). See §7. |
| `~/.dsh/llm-deepseek/files-v3.json` | Provider file-upload bookkeeping — irrelevant. |
| `~/.dsh/.credentials.yaml` | Secrets — **do not read**. |

### 1.2 Filename / generation scheme

`dsh-session-format/lib/index.js:464-489`:

```js
const CANONICAL_LOG_FILENAME = /^session(?:\.v([1-9][0-9]*))?\.jsonl$/u;
function sessionFormatLogFilename(version) {
  return version === 0 ? "session.jsonl" : `session.v${version}.jsonl`;
}
```

With the shipped `compression: 'zstd'` default (`dsh-session-persistence-jsonl/README.md:48`), the artifact is `session[.vN].jsonl.zstd`. v0 is `session.jsonl.zstd`; every later generation is `session.vN.jsonl.zstd`. A reader must pick the **numerically highest** generation present and check `header.version` equals `N`; refuse (don't guess) if `header.version > 4`.

Layout (`dsh-session-persistence-jsonl/README.md:57-73`):

```
<root>/
  --<projectKey>--/      or _no-cwd/
    <encodeSegment(sessionId)>/
      session.v4.jsonl.zstd
```

### 1.3 Project slug encoding (the `--...--` directory)

`dsh-session-persistence-jsonl/lib/index.js:858-899`:

```js
function encodeSegment(raw) {           // session directory name
  if (raw === ".") return "~002E";
  if (raw === "..") return "~002E~002E";
  ... if (ch !== "~" && /^[A-Za-z0-9._-]$/.test(ch)) out += ch;
      else out += "~" + code.toString(16).toUpperCase().padStart(4, "0");
}
function projectKey(cwd) {              // project directory name
  ... if (ch === "/" || ch === "\\" || ch === ":") { if (!separatorRun) readable += "-"; separatorRun = true; }
      else if (ch !== "~" && /^[A-Za-z0-9._-]$/.test(ch)) readable += ch;
      else readable += "~XXXX";
  return `--${(readable.replace(/^-+/, "") || "root").slice(0, 251)}--`;
}
```

Consequences:

- Runs of separators/colon collapse to one `-`; leading dashes stripped; empty → `root`; truncated to 251 chars (lossy, do not try to invert it — use `header.cwd` instead).
- Non-`[A-Za-z0-9._-]` code units become uppercase 4-hex escapes. Real example:
  `C:\Users\liujj\Dropbox\我的文档\Job Applications\claude-resume-and-hiring` →
  `--C-Users-liujj-Dropbox-~6211~7684~6587~6863-Job~0020Applications-claude-resume-and-hiring--`
  (`我`=U+6211, `的`=U+7684, `文`=U+6587, `档`=U+6863, space=U+0020).
- Session directories are normally the session id verbatim (ids are already safe), except `.`/`..`/`~`.

### 1.4 Physical framing (the important part)

`dsh-session-persistence-jsonl/README.md:111`:

> The default artifact is a standard concatenation of independent Zstandard frames: one checksummed frame containing only the header line, then one checksummed frame per durable append batch, using Node's built-in Zstandard API at its default compression level (no level knob). The current format writes one event per row.

Verified empirically (Python `zstandard` 0.23, session `session-fa302a65…`, 1,069,134 bytes at the time):

```
frame 0: out=215   first=b'{"type":"session","version":4,...'      <- header only
frame 1: out=261   first=b'{"type":"permission/preset","seq":0,...'
frame 2: out=650
frame 3: out=83862 <- many JSONL lines, one flush batch
frame 4: out=1291
frame 5: out=1336
```

So: **one frame can contain many events**; **every frame decompresses independently** (no shared dictionary/context, no dependence on earlier frames). Frame header descriptor `0x04` = checksum flag set, no content-size, no dict id, not single-segment.

### 1.5 Append / batching semantics

- Live-event batching window: **200 ms** — `dsh-session-persistence-jsonl/lib/index.js:187-194`:
  ```js
  enqueueLive(event, reportBackgroundFailure) {
    this.buffered.push(structuredClone(event));
    if (this.batchTimer !== void 0 || this.drainPaused) return;
    this.batchTimer = setTimeout(() => { this.batchTimer = void 0; this.drainLive().catch(reportBackgroundFailure); }, 200);
  }
  ```
- Every flush writes **one compressed frame** (or one block of lines when `compression:'none'`) and `fsync`s before the append resolves; a failed write/sync rolls the file back to its prior length (`README.md:77`).
- Forced flushes (checkpoints) beyond the 200 ms timer — `dsh-session-checkpoint-policy/README.md`: before the model request, before a top-level tool body, and at every `agent/pre-step` boundary. That is why frames cluster around step boundaries and immediately before `request/header`.
- **Committed events are never rewritten**; the file is append-only for its generation. There is no rotation, no log truncation, no in-place compaction.
- The whole file is only "rewritten" when a **format migration** publishes a new generation (`session.v3 → session.v4`): a new file is created via temp-write + exclusive publish, and the old file stays byte-identical (`README.md:87`).
- Deletion: **no seam deletes session files** (`README.md:167` "Nothing deletes session files"). Archiving is only a flag in `workspace.json`.
- Crash / torn tail (`README.md:77`): an interrupted final frame contributes only its **complete** JSONL records; the writer truncates the torn bytes and rewrites those recovered records before its next batch. An incomplete raw line (compression:none) is discarded.
- A session that is created but never written leaves **nothing** on disk; an explicit `flush()` of an empty session writes a header-only file (one frame, 0 events).

### 1.6 File growth, measured

31 logs: 22–501 frames, 4–856 events, 0.3 KB–1.24 MB compressed, 0.4 KB–4.22 MB uncompressed (~8:1). A frame holds between 1 and ~180 events.

---

## 2. Event envelope & session header

### 2.0 Header line (`type:"session"`, no `seq`, no `data`)

```jsonc
{
  "type": "session",
  "version": 4,
  "id": "session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7",
  "createdAt": 1794753207647,          // epoch ms
  "cwd": "E:\\myAIProjects\\zetrix-dsh-screen",   // optional; absolute
  "parentSession": "session-…",        // optional; only subagent children
  "isSeeded": false,                   // required boolean
  "origin": "subagent",                // optional; the only admitted value
  "delegationDepth": 0,                // required non-negative int
  "agentPreset": "standard"            // optional string
}
```

Declaration (`SessionHeader`): `version, id, createdAt, cwd?, parentSession?, isSeeded, origin?:'subagent', delegationDepth?, agentPreset?`.
The v4 header validator requires `createdAt`, `isSeeded`, `delegationDepth` (`dsh-session-format/lib/index.js:76-85`); in practice all logged headers carry all non-optional fields (2 key-set variants observed: 8 keys, and 10 keys when `origin`+`parentSession` are present).

`id` **is the session identity**; the directory name is just `encodeSegment(id)`.

---

## 3. Event catalogue

Format per event: writer → exact `data` shape → semantics. `type`, `seq`, `time` are omitted from the shape blocks (they are always `{type:string, seq:number, time:number, data:object}`).

### 3.1 Turn / step lifecycle — `dsh-agent-loop`

#### `turn/start`
```ts
data: { turn: number }            // 1-based, strictly +1 per turn in the session
```
Appended at the top of `ReactLoopAgent.turn()` (`dsh-agent-loop/lib/index.js:731`) after the loop claimed the driver. Opens the model-visible turn; every subsequent message/tool event carries `turn`.

#### `turn/end`
```ts
data: {
  turn: number,
  reason: TurnEndReason
}
type TurnEndReason =
  | { kind: 'completed' }
  | { kind: 'aborted';  reason: TurnEndCancelCause }
  | { kind: 'blocked' }
  | { kind: 'error';    error: LlmFailure }
  | { kind: 'max-tokens' }
  | { kind: 'interrupted' }
  | { kind: 'forked' }

type TurnEndCancelCause = AgentCancelCause | { kind: 'legacy' };
type AgentCancelCause   = {kind:'user'} | {kind:'parent'} | {kind:'hook', reason:string} | {kind:'disposed'};

interface LlmFailure { message: string; code: string; status?: number;
                       providerRetryAfterMs?: number; requestId?: string; offloadImages?: number }
```
Written in a `finally` block (`dsh-agent-loop/lib/index.js:817-826`), so it is appended on every normal exit including throw/abort. Real corpus: 72 `turn/end` → 66 `{"kind":"completed"}`, 6 `{"kind":"aborted","reason":{"kind":"user"}}`.

Reason production (same file):

- `completed` — step returned `{kind:'completed'}` and no queued next-step input remained (`:751-758`, `:958`, `:973`).
- `blocked` — `agent/pre-step` returned `{kind:'reject'}` (`:746-749`): e.g. the batch was rejected by a pre-step listener; nothing was executed.
- `max-tokens` — the provider finished with `finish.kind === 'max-tokens'` (`:956`).
- `aborted` — the turn's `AbortSignal` was aborted; the reason is copied by `abortedCancelCause()` (`:495-509`), which keeps only `kind` (and `hook.reason`):
  ```js
  switch (cause.kind) {
    case "user": case "parent": case "disposed": return { kind: cause.kind };
    case "hook": return { kind: "hook", reason: cause.reason };
  }
  ```
  `user` = the Stop button / `agent.cancel({kind:'user'})`; `parent` = owning subagent/team parent cancelled it; `disposed` = session/agent torn down; `legacy` = older logs.
- `error` — any other throw (`:809-816`): `{kind:'error', error: <LlmFailure>}` for `LlmError`, else `{message, code}` where `code` is `ToolActivationError.code` or `"UNKNOWN"`.
- `interrupted` — **synthetic**, appended by crash repair; see §3.1.1.
- `forked` — **synthetic**, appended by fork-seed construction (`buildForkSeed`) to close the inherited tail of a forked child.

#### `step/start` / `step/end`
```ts
data: { turn: number, step: number }    // step is 1-based and resets to 1 on each new turn
```
`step/start` is appended only when the step actually has input (`dsh-agent-loop/lib/index.js:751-760`): a turn that is immediately blocked/completed **has no step**. `step/end` is in a `finally` (`:779-785`), so it exists for every `step/start` except a crash.

#### 3.1.1 Crash tails and synthetic closers — `dsh-session/lib/types/repair.js`

If a process dies with an open turn, the log simply **ends inside the turn** (no `turn/end`). On the next open, `openTurnClosers(events,'interrupted')` appends, in order:

1. synthetic `tool/result` for every unanswered tool call (`error.code = TOOL_OUTCOME_UNKNOWN`, or `TOOL_NOT_STARTED` if the call never got a `tool/call` record),
2. `step/end` if a step was open,
3. `turn/end` with `{kind:'interrupted'}` (`repair.js:76`).

```js
closers.push({ type: 'turn/end', seq: SessionSeq(seq++), time, data: { turn: openTurn, reason: { kind: cause.kind } } });
```

Note the closers reuse the **last real event's timestamp** and continue `seq`. So `turn/end.reason.kind === 'interrupted'` unambiguously means "the previous process died mid-turn" (and `forked` means "this is a fork seed").

### 3.2 Messages

#### `user/message`
`data` **is** the `UserMessage` itself (no wrapper):
```ts
data: {
  id: string,                      // MessageId
  role: 'user',
  content: ContentBlock[],         // text | image | file
  source: MessageSource
}
// envelope also: surfaceOp: 'append' | {op:'replace',startSeq,endSeq}, sourceEventSeqs?: number[]
```
Source union actually observed / declared (`MessageSourceMap`), all durable:

| `source` | Written when |
|---|---|
| `{kind:'user', rpcId, clientTimeZone?}` | real typed prompt (client RPC) |
| `{kind:'user'}` | programmatic/user-role message without RPC |
| `{kind:'runtime-context', form:'snapshot', sections:[{name,text}]}` | per-step runtime context snapshot |
| `{kind:'time-context', form:'snapshot', sections}` | time injection (same shape) |
| `{kind:'model-selection', form:'notice', summary}` | model switch notice |
| `{kind:'skill-catalog', form:'catalog', entries}` | skill catalog injection |
| `{kind:'agent-instructions', form:'instructions', …}` | AGENTS.md etc. |
| `{kind:'user-approval', form:'notice'\|…}` | approval policy switch notice |
| `{kind:'compact-checkpoint', compactionId, sourceCommandId?}` | **compaction replacement message** (see §3.8) |
| `{kind:'agent-message', form:'relay', senderSessionId}` | inter-agent relay |
| `{kind:'subagent-settled', form:'notice', summary, senderSessionId}` | child settlement notice |
| `{kind:'tool-jobs', form:'notice', …}` | background-job notice |
| `{kind:'user-question-reply', callId, outcome:'answered'}` | answer to a *timed* pending question (body is JSON `{kind:'answer_to_pending_question', tool:'ask_user_question', callId, questions, answers}`) |
| `{kind:'goal', goalId, revision, round}` · `{kind:'plan-mode'}` · `{kind:'schedule'}` · `{kind:'working-directory'}` · `{kind:'team-message'}` · `{kind:'session-reference', form:'recall'}` | other plugins |

Observed counts: `user` 76, `time-context` 57, `model-selection` 38, `runtime-context` 32, `skill-catalog` 29, `agent-instructions` 15, `agent-message` 7, `subagent-settled` 7, `tool-jobs` 7, `user-approval` 2, `compact-checkpoint` 1.

`MessageSourceMap` is **merge-extensible** (each package adds its own kinds), so the canonical map extracted from the app's host types contains only core kinds; `time-context`, `skill-catalog`, `agent-instructions` and `tool-jobs` above are plugin-contributed sources that appear in real logs. For the display: only `kind == "user"` means a real human prompt; `user-question-reply` means an answer arrived; everything else is harness-injected context and should not be counted as user activity.

#### `system/message`
```ts
data: { turn: number, step: number,
        message: { id: string, role: 'system', content: ContentBlock[],
                   source: { kind: 'system-prompt' } } }
// envelope: surfaceOp
```
One per prompt (re)write; with `systemPromptUpdate:'in-history'` the prompt lives in history and is appended here.

#### `developer/message`
```ts
data: { turn: number, step: number,
        message: { id: string, role: 'developer',
                   content: ({type:'tool-addition'}|{type:'tool-removal'})[],
                   source: { kind: 'tool-registry' } },
        headerSeq?: number }      // seq of the request/header that added tools
// envelope: surfaceOp
```
Emitted when the tool set changes between requests (`dsh-agent-loop/lib/index.js:1045-1064`). Only 1 in the corpus.

#### `assistant/message`
```ts
data: {
  turn: number, step: number,
  message: { id: string, role: 'assistant',
             content: ContentBlock[],          // reasoning | text | tool-call | image
             source: { kind:'model', provider:string, model:string,
                       replayState?: unknown,      // present in 865/1015 rows
                       plugin?: string } },        // e.g. 'codingns4dsh' mirror
  stream: AssistantStreamRecord[],                 // compact replay stream (see below)
  usage?: TokenUsage,                              // absent when adapter reported none
  interrupted?: true                               // cancelled stream: durable visible prefix
}
// envelope: surfaceOp
```
`AssistantStreamRecord`:
```ts
{ type:'text-chunks',      time0, index, dt:number[], texts:string[] }
{ type:'reasoning-chunks', time0, index, dt:number[], texts:string[] }
{ type:'tool-call-chunks', time0, index, dt:number[], id, name?, args:string[] }
{ type:'chunk',            time, chunk: StreamChunk }
```
`StreamChunk`: `block-start | text-delta | reasoning-delta | tool-call-delta | block-end | usage {usage} | finish {reason, replayState?}`.

Semantics: the authoritative completion record of one model call. `interrupted:true` means the user cancelled while content was streaming; `content` holds what the user saw, `usage` is whatever arrived. Block types observed: `tool-call` 1374, `reasoning` 816, `text` 162.

#### `assistant/attempt`
```ts
data: { turn: number, step: number, stream: AssistantStreamRecord[] }
```
The **log-only settlement when no message could be committed** — a stream that failed/aborted, and the "usage-only" settlement written by external adapters (codingns). 159 rows in corpus, all with a single `{type:'chunk', chunk:{type:'usage', …}}` element. It never contributes model-visible content; the resume docs say "embedded Assistant streams and log-only attempts do not duplicate messages". For our purposes it is a **secondary usage source** (often the *richest* one, see §5.4/§F).

### 3.3 Tools

#### `tool/call` — `dsh-tools/lib/pipeline.js:185-195`
```ts
data: { turn: number, step: number,
        callId: string,          // provider tool-call id (or synthetic id)
        name: string,            // tool name, open vocabulary (see §6.3)
        arguments: string }      // RAW JSON text exactly as the model emitted it (never an object)
```
Written **before** pre-execute policy / approval / the tool body (`startCall` calls `settlement.start()` first, `pipeline.js:104-108`), so it exists even when the call is later denied or aborted.

#### `tool/result` — `dsh-tools/lib/pipeline.js:196-213`
```ts
data: {
  turn: number, step: number,
  message: { id: string, role: 'tool', toolCallId: string,
             isError?: boolean,
             content: ContentBlock[],
             source: { kind:'tool', callId:string } },
  error?: { name: string, code: string, reason?: string },
  meta?: JsonValue                    // e.g. {diffs:[{path, oldText, newText}]} from file tools
}
// envelope: surfaceOp:'append', sourceEventSeqs:[callSeq]
```
Written for **every** started call, including rejected/denied/aborted ones (`pipeline.js:220-235` synthesizes `{"text":"Error: tool call aborted before dispatch"}`, `error.code='ABORTED_BEFORE_DISPATCH'`), so `tool/call` and `tool/result` pair 1:1 — except across a crash (then repair adds the missing result, §3.1.1).

Real aggregates: 1,318 `tool/call` / 1,316 `tool/result` while scanning a live corpus (the delta was my own in-flight calls). `error` present in 36 results; `meta` in 1,090.

### 3.4 Approval, permission, sandbox

#### `approval/policy` — `dsh-user-approval/lib/index.js:82-85`
```ts
data: { policy: 'ask' | 'never', source?: 'delegation' }
```
Whole-value replace; last one wins. Written once during session init (value `ask` or `never`) and again on every user/plugin policy switch. `source:'delegation'` = inherited from a parent agent.

#### `approval/asked` / `approval/decided` — `dsh-user-approval/lib/index.js:153-168`
```ts
approval/asked:   { id: string,            // ApprovalRequestId (uuid)
                    toolName: string,
                    callId?: string,        // tool call that triggered it
                    reason?: string }       // free text; sandbox escalation format below
approval/decided: { id: string,
                    outcome: 'allowed-once' | 'rejected' | 'cancelled' | 'unavailable' }
```
Both are appended **synchronously** inside `request()` around the answerer await; the pair is turn-enclosed by construction:

```js
if (!this.approvalStateOf(session).turnOpen) throw new Error("approval.request() outside an open turn ...");
session.append("approval/asked", {...});
const outcome = await this.decide(req, session);
session.append("approval/decided", { id, outcome });
```

`allowed-once` is the only grant. `rejected` = policy `never` or the user said no; `cancelled` = the turn signal aborted while waiting; `unavailable` = no answerer / answerer threw (fail closed).

Sandbox-escalation reason format is fixed by `dsh-sandbox/lib/index.js:154-160`:

```js
reason: `escalate sandbox to ${mode}: ${justification}`,
```

`justification` is a model-supplied tool argument (`sandbox_permissions` + `justification`, `dsh-tool-pwsh/lib/index.js:631`). Real row: `"escalate sandbox to danger-full-access: plugin_manager {\"action\":\"install_bundle\",...}"`.

#### `permission/preset` — `dsh-permission-presets/lib/index.js:364`
```ts
data: { preset: string }        // configured names; shipped: 'workspace-write' | 'danger-full-access'
                                // reserved derived values: 'custom', 'auto'
```
Only appended when the resolved preset actually changes. `workspace-write` 22, `danger-full-access` 5 observed.

#### `sandbox/mode` — written by the sandbox/permission layer
```ts
data: { mode: 'read-only' | 'workspace-write' | 'danger-full-access', source?: 'delegation' }
```

Startup order is fixed: `permission/preset` → `sandbox/mode` → `approval/policy` → (`session/end-seed` when seeded) → first `agent/inbox/spliced` → `turn/start`.

### 3.5 Request / model / context

#### `request/context` — `dsh-agent-loop/lib/index.js:1066-1075`
```ts
data: { provider: string, model: string,
        contextWindow?: number,
        systemPromptUpdate?: 'in-history' }
```
**Not per request.** Appended only when one of those four values differs from the latest snapshot. It is therefore a *state* record: the last one describes the current route and its window. 28 rows for 1,015 assistant messages.

#### `request/header` — `dsh-agent-loop/lib/index.js:1029-1044`
```ts
data: {
  header: {
    config: { provider: string, model: string, maxTokens?: number, reasoningEffort?: string, … },
    adapterDefaults?: { reasoningEffort?: true, maxTokens?: true },
    tools?: ToolSchema[]            // FULL tool schemas — often 30–60 KB
    // system?: never  (the prompt is history, not header)
  },
  reason: 'initial' | 'resume' | 'change' | 'series',
  startsSeries?: true
}
```
Full header on first request, on a changed envelope (config or tools), after a surface replacement, on resume, and on a series start; unchanged steps inherit the previous header. **Skip `data.header.tools` for state** (it is the single largest field in the log), unless you need to detect the timed `ask_user_question` schema.

#### `model/selection` — `dsh-agent/lib/types/model-selection.js:91`
```ts
data: { provider: string, model: string, reasoningEffort?: string }
```
Whole-value replace, appended by `agent.model.select()` when the selection actually changes; last one wins. 13 rows (all without `reasoningEffort` in this corpus).

### 3.6 Titles

#### `session/title` — `dsh-session-title/lib/index.js`
```ts
data: {
  title: string,
  messageSeqs: number[],            // source human-message seqs; [] for a manual rename
  source: {kind:'fallback'} | {kind:'provider', provider:string, model?:{provider,model}} | {kind:'user'}
}
```
Fold = **last** `session/title` wins (whole-value replace). Written by:

- `appendFallback`/`ensureFallback` (`:619-651`) — deterministic first-prompt fallback: `fallbackSessionTitle(first.text, fallbackMaxWords=5, fallbackMaxBytes=40)` (`dsh-base/cordis.patch.yml`: `fallbackMaxWords: 5`, `fallbackMaxBytes: 40`, `maxTitleBytes: 80`).
- the LLM provider (`:480-490`) — rewrites the fallback with a model-generated title, `source:{kind:'provider', provider:'session-title-first-prompt-llm', model}`.
- **manual rename** (`:301-317`):
  ```js
  rename(session, title) {
    ...
    const normalized = normalizeSessionTitle(title, this.config.maxTitleBytes);
    if (normalized.length === 0) throw new SessionTitleInvalidError("session title must contain visible characters");
    const state = this.stateFor(session);
    this.supersede(state, "user rename superseded automatic title generation");
    session.append("session/title", { title: normalized, messageSeqs: [], source: { kind: "user" } });
  }
  ```
  There **is** a rename API (`dsh-api-session-controller/lib/types/commands.js:186` `rename()` → `titles.rename(...)`, exposed as a session RPC); it writes exactly this record. Once `source.kind === 'user'`, automatic generation stops (pinned) — `:394`.

#### `session/title-llm-request` — `dsh-session-title-llm/lib/index.js:199-208`
```ts
data: {
  titleProvider: string,                 // 'session-title-first-prompt-llm' | plugin ids e.g. 'codingns-title-optimization'
  messageSeqs: number[],
  route: { provider: string, model: string },
  system: string,                        // exact system prompt of the auxiliary call
  messages: UserMessage[],               // exact single user message
  maxTokens: number,
  reasoningEffort?: string
}
```
**Log-only audit record**, appended immediately before the auxiliary LLM stream; it is not a title. `session/title` that follows carries the result. Observed: 35 requests / 49 titles (fallback first, then provider rewrite).

### 3.7 Todos and progress

#### `todo/write` — `dsh-tool-todo/lib/index.js:173-176`
```ts
data: { todos: { content: string, status: 'pending' | 'in_progress' | 'completed' }[] }
```
- Whole-list replacement snapshot (never a delta), one per `todo_write` tool call.
- **No `cancelled` status** — the enum is exactly `pending | in_progress | completed` (`:14-18`, `:66-71`).
- The *projection* resets on every turn: `apply: (state,event) => { if (event.type==='todo/write') return event.data.todos; if (event.type==='turn/start') return null; return state; }` (`:85-89`). So the log keeps every write, but the effective list is the last `todo/write` **after the current turn's `turn/start`** (or empty).
- Constraints: trimmed non-empty unique `content`; at most one `in_progress` unless the deployment allows parallel (`allowParallelInProgress`).

#### Agent-reported progress (percent)
**There is no percent/progress event in the session log.** Verified by a recursive key scan of all events (no `progress`/`percent` key exists anywhere in `data`). What exists instead:

- `todo/write` snapshots → derive `completed/total` counts yourself.
- `sessionStats` in the projection cache: `{turns, steps, pendingCalls, llmMs, toolMs, ttftMs, decodeMs, decodeTokens}` — counters, not percent.
- `job` objects have a live `progress` field (`dsh-jobs-local/lib/index.js:589`), but it is **not written to `job/registered|settled`** — RPC-only.
- `turnOutline` (projection cache): per-turn `{turn, seq, prompt, response}` — useful for a "current turn started with …" line.

Do not invent a percentage; if the display needs one, define it as `completed_todos / total_todos` and label it as such.

### 3.8 Compaction — `dsh-compaction-basic/lib/index.js`

```ts
compaction/start:   { compactionId: string, sourceCommandId?: string, turn: number | null }
compaction/summary: { compactionId, sourceCommandId?,
                      summary: ContentBlock[],            // the replacement text (text blocks)
                      shadowedRange: {start:number, end:number},
                      shadowedSeqs: number[],
                      shadowedTokenCount: number,
                      provider: string, model: string,
                      maxTokens?: number, usage?: TokenUsage,
                      rawOutput?: ContentBlock[], llmStreamCall?: true }
compaction/end:     { compactionId, sourceCommandId?, turn: number | null, error?: string }
```

Sequence on success: `compaction/start` → (summarizer attempts) → `compaction/summary` → **`user/message` checkpoint** → `compaction/end`.

The checkpoint message is the surface mutation (`dsh-compaction/lib/types/checkpoint.js`):

```jsonc
{"type":"user/message","seq":264,
 "data":{"id":"codingns-compaction-01a1df72-…","role":"user",
         "content":[{"type":"text","text":"Codex 已压缩较早的上下文。"}],
         "source":{"kind":"compact-checkpoint","compactionId":"01a1df72-…"}},
 "sourceEventSeqs":[[10,14],22,24,…],
 "surfaceOp":{"op":"replace","startSeq":10,"endSeq":257}}
```

Semantics:

- `compaction/start` is the compaction lock; **every later failure makes exactly one `compaction/end` attempt** with `error` set — "a failed close deliberately leaves the unmatched start detectable" (`:441-448`). So an unmatched `compaction/start` (no `compaction/end`) = interrupted compaction.
- `compaction/summary` + the checkpoint replace the whole `[startSeq,endSeq]` surface range; for a disk-only reader this means "earlier messages are no longer in the model's context" — token/context estimates must be based on the post-compaction state, not on a sum over all history.
- `shadowedTokenCount` may be `0` when the backend reports its own compaction (observed for the codex plugin path).
- The turn field is `null` only for manual `/compact` outside a turn.

### 3.9 Subagents

Child session (`dsh-subagent/lib/index.js:353-356`):
```jsonc
{"type":"session","version":4,"id":"0f8d380d-cd24-4cfd-8fba-603680aafa33",  // BARE uuid
 "createdAt":1794753678161,"cwd":"E:\\myAIProjects\\zetrix-dsh-screen",
 "isSeeded":false,"delegationDepth":1,"agentPreset":"standard",
 "parentSession":"session-fa302a65-ee07-4c7a-9ff3-36bae321e1d7","origin":"subagent"}
```

#### `subagent/descriptor` — first event (`seq:0`) of a child session
```ts
data: { version: number, mode: 'one-shot' | 'continuable', provider: string,
        label?: string,                    // required for continuable
        agentProvider?: string, agentModel?: string,
        agentReasoningEffort?: string, persona?: string, toolFilter?: unknown }
```

#### `subagent/catalog` — in the **parent** session
```ts
data: { version: 0|1|2, childId: string, childCreatedAt: number,
        mode: 'one-shot'|'continuable'|'external'|'unknown', label?: string }
```

### 3.10 Jobs (background tool jobs) — `dsh-jobs-local/lib/index.js:604-624`

```ts
job/registered: { jobId: string, owner: string,        // owner = session id (bare uuid for children)
                  kind: string,                        // 'pwsh' | 'bash' | … producer kind
                  label: string, startedAt: number,
                  toolCall?: { callId: string, rootCallId: string } }
job/settled:    { …same…, finishedAt: number,
                  status: 'completed' | 'killed' | 'failed',
                  cause: 'producer' | 'kill' | 'teardown',
                  producerReported: boolean,
                  detail?: string }                    // e.g. "exit code: 0"
```
Only for jobs owned by a session. 327 registered / 323 settled at first scan; a stale unmatched `job/registered` can survive a crash (`job/settled` is skipped if the process dies), so **pending job ≠ proof of working**.

### 3.11 Commands — `dsh-commands/lib/index.js:334-380`
```ts
command/run:  { commandId: string, name: string, args?: string,   // args omitted when recordInput===false
                source: {kind:'user'} }
command/done: { commandId: string, kind: 'success' | 'error', text?: string, sourceEventSeq?: number }
```
Both are **log-only**, written outside any turn (`/plan`, `/permission`, `/goal`, `/compact`, …). `commandId` format: `cmd-<instanceToken>-<n>`. Observed: `{"commandId":"cmd-de1f7c1-1","name":"permission","args":" danger-full-access"}` → `{"kind":"success","text":"preset danger-full-access"}`.

### 3.12 Session-level misc

#### `session/end-seed` — `dsh-session/lib/index.js:1423-1430`
```ts
data: { inherited?: true }
```
Seed-boundary marker. Two uses:

- In a **seeded** session (`header.isSeeded === true`, fork), the marker at index `inheritedEventCount` carries `{inherited:true}` and separates copied parent history from child-owned events.
- On **every** session construction/resume, if the seed is non-empty and the last event is not already a marker, `{}` is appended at the cut. That is why some logs end with a trailing `{"seq":438,"type":"session/end-seed","data":{}}` right after `turn/end` — a resume happened and nothing followed. `state` is unaffected.

```js
if (seed !== void 0 && mode === "snapshot" && this.header.isSeeded && !markedSeed) this.append("session/end-seed", { inherited: true });
else if (seed !== void 0 && !(mode === "snapshot" && this.header.isSeeded) && this.log.at(-1)?.type !== "session/end-seed") this.append("session/end-seed", {});
```

#### `session-log-deepseek/delivery-accepted` — `dsh-session-log-deepseek/lib/index.js:190-196`
```ts
data: { sessionId: string, sessionFormatVersion?: number, throughSeq: number }
```
Appended when the official DeepSeek API returned 2xx for a request that carried a `dsh_session_log` suffix. It is an **upload watermark**, not a durability or state signal: the *last written line* is the real durability frontier, and `throughSeq ≤ seq`. The same `throughSeq` can be accepted more than once (concurrent deliveries), so 786 rows ≠ 786 requests. Folding max(`throughSeq`) per (sessionId, formatVersion) is what DSH does; our reader can ignore these rows entirely.

#### `agent/inbox/spliced` — `dsh-agent-loop/lib/index.js:189-198`
```ts
data: { target: 'next-turn' | 'next-step',
        start: number,
        removedCount?: number,       // omitted when 0
        inserted: UserMessage[],     // full message objects
        outcome?: 'canceled' }       // set when removed messages were discarded
```
Durable record of the queued-input list. Insert with `inserted` non-empty = a user message (or a notice/relay) is **queued but not yet admitted** to the transcript; it becomes a `user/message` at the next step/turn boundary. `removedCount` + `outcome:'canceled'` = queued input discarded (stop button / archive). Observed: 283 rows, 270 with `removedCount:1`, 5 `outcome:"canceled"`, 30 with a real `inserted` message.

#### `deliverables/presented` — `dsh-tool-present/lib/index.js:118-123`
```ts
data: { turn: number, callId: string,
        files: { path: string, description?: string }[] }   // path is the resolved absolute path
```
Appended on the **successful** tool result of a `present` call (`turn` = the turn number captured when the tool body ran). Only durable evidence of "files delivered".

#### `workspace/changes`
```ts
data: { turn: number }
```
A **pointer only** (`dsh-workspace-changes/lib/types/recorder.js:282`). The changed-file summary/diffs live in memory keyed by the event `seq` and are served over RPC; **a disk-only reader cannot recover the file list**. Emitted once per turn that changed files.

### 3.13 Known types not observed in this corpus (reader must tolerate)

`plan/mode {active:boolean}`, `goal/change` (see §5.2 refinements), `image/offload {targets}`, `llm/retry`, `llm/retry-started`, `working-directory/change {cwd}`, `agent-preset/selected {agentPreset}`, `compaction/prune`, `tool/ptc-dispatch`, `tool/ptc-dispatch-start`, `team/member`, `team/task`, `team/message/queued`, `team/message/delivered`, `feedback/record`, `feedback/message-put`, `feedback/message-delete`, `hook/invoked`, `hook/result`, `schedule/change`, `web/deepseek-search-llm-request`, `subagent/model-selection-policy`, `tool-workflow/*`.

---

## 4. Complete `SessionEventMap` (authoritative declaration)

Extracted verbatim from `dsh-api-session-controller/lib/typert.host.js` (`"name": "SessionEventMap"`), abbreviated to the events this reader cares about; `…` marks elided union members documented above.

```ts
interface SessionEventMap {
  'turn/start': { turn: number };
  'turn/end': { turn: number; reason: TurnEndReason };
  'step/start': { turn: number; step: number };
  'step/end': { turn: number; step: number };
  'user/message': UserMessage;
  'developer/message': { turn: number; step: number; message: DeveloperMessage; headerSeq?: SessionSeq };
  'system/message': { turn: number; step: number; message: SystemMessage };
  'assistant/message': { turn: number; step: number; message: AssistantMessage;
                         stream: AssistantStreamRecord[]; usage?: TokenUsage; interrupted?: true };
  'assistant/attempt': { turn: number; step: number; stream: AssistantStreamRecord[] };
  'tool/call': { turn: number; step: number; callId: ToolCallId; name: string; arguments: string };
  'tool/result': { turn: number; step: number; message: ToolResultMessage;
                   error?: { name: string; code: string; reason?: string }; meta?: JsonValue };
  'request/header': { header: EpochHeader; reason: RequestHeaderReason; startsSeries?: true };
  'request/context': RequestContext;
  'session/end-seed': { inherited?: true };
  'agent/inbox/spliced': { target: InboxTarget; start: number; removedCount?: number;
                           inserted: UserMessage[]; outcome?: 'canceled' };
  'model/selection': ModelSelection;
  'sandbox/mode': { mode: SandboxMode; source?: 'delegation' };
  'agent-preset/selected': { agentPreset: string };
  'job/registered': JobRegisteredData;
  'job/settled': JobSettledData;
  'command/run': { commandId: CommandId; name: string; args?: string; source: CommandSource };
  'command/done': { commandId: CommandId; kind: 'success' | 'error'; text?: string; sourceEventSeq?: SessionSeq };
  'image/offload': { targets: ImageOffloadTarget[] };
  'session/title': SessionTitleEventData;
  'todo/write': { todos: TodoItem[] };
  'working-directory/change': { cwd: string };
  'approval/asked': { id: ApprovalRequestId; toolName: string; callId?: ToolCallId; reason?: string };
  'approval/decided': { id: ApprovalRequestId; outcome: ApprovalOutcome };
  'approval/policy': { policy: ApprovalPolicy; source?: 'delegation' };
  'permission/preset': { preset: string };
  'subagent/descriptor': SubagentDescriptorData;
  'subagent/catalog': SubagentCatalogEvent;
  'compaction/start': { compactionId: CompactionId; sourceCommandId?: CommandId; turn: number | null };
  'compaction/summary': { compactionId; sourceCommandId?; summary: ContentBlock[];
                          shadowedRange: {start: SessionSeq; end: SessionSeq};
                          shadowedSeqs: SessionSeq[]; shadowedTokenCount: number;
                          provider: string; model: string; maxTokens?: number; usage?: TokenUsage } &
                        ({ rawOutput: ContentBlock[]; llmStreamCall: true } | { rawOutput?: ContentBlock[]; llmStreamCall?: never });
  'compaction/end': { compactionId; sourceCommandId?; turn: number | null; error?: string };
  'compaction/prune': { shadowedRange; shadowedSeqs; shadowedTokenCount };
  'plan/mode': { active: boolean };
  'goal/change': GoalChangeMeta;
  'session-log-deepseek/delivery-accepted': { sessionId: SessionId; sessionFormatVersion?: number; throughSeq: SessionSeq };
  'workspace/changes': { turn: number };
}
```

---

## 5. State derivation — the display contract

### 5.1 Fold state (single pass, O(events), no memory of content)

```
openTurn: Option<u32>            // turn/start => Some(turn); turn/end{same turn} => None
openStep: Option<u32>
pendingCalls:   Map<callId, (turn, step, name)>   // tool/call minus tool/result
pendingApprovals: Set<approvalId>                  // approval/asked minus approval/decided
pendingQuestions: Map<callId, 'open'|'continued'>  // name == 'ask_user_question'
lastTurnEnd: Option<Reason>
todos: Option<Vec<Todo>>         // reset to None on turn/start, replaced by todo/write
title: Option<session/title.data>
usage: Option<(seq, TokenUsage, provider, model)>  // newest of assistant/message|assistant/attempt
contextWindow: Option<u32>       // from newest request/context
lastEventTime: u64; lastSeq: u32
```

### 5.2 Precedence table (evaluate top-down)

| # | Condition (log-derived) | Display state | Payload |
|---|---|---|---|
| 1 | no readable header | `unknown` | — |
| 2 | `pendingApprovals` non-empty (⇒ open turn) | **waiting-approval** | `toolName`, `reason` of the newest unmatched `approval/asked`; elapsed = now − its `time` |
| 3 | open turn AND `pendingQuestions` has an `'open'` entry | **waiting-question** | question text from `tool/call.arguments` (parse JSON, `questions[].question/header/options`); elapsed |
| 4 | open turn AND `pendingCalls` contains `exit_plan_mode` | **waiting-plan-review** | the Markdown plan is a field of `tool/call.arguments` (plugin not mounted in this build) |
| 5 | open turn | **working** | sub-label: `openStep.is_some()` → "step N"; if `pendingCalls` non-empty → running tool `name`; else "thinking/streaming" (assistant stream is not logged until it settles) |
| 6 | no open turn, `pendingQuestions` has `'continued'` (timed mode only) | **done (answerable)** | unanswered question still open |
| 7 | no open turn, last reason `error` | **failed** | `error.message`, `error.code`, `status?` |
| 8 | no open turn, last reason `blocked` | **blocked** | pre-step rejected the batch; nothing ran |
| 9 | no open turn, last reason `aborted` (`reason.kind` user/parent/disposed/legacy/hook) | **stopped** | distinguish `user` (stop button) vs `parent`/`disposed` |
| 10 | no open turn, last reason `interrupted` | **stopped (crashed)** | process died mid-turn; a later resume appended the closer |
| 11 | no open turn, last reason `completed` or `max-tokens` or `forked` | **done** | `max-tokens` → "done (output limit)" |
| 12 | no `turn/start` at all | **idle/blank** | header only, or only startup + title events |

Refinements that matter on an e-ink display:

- **Staleness**: if the computed state is `working`/`waiting-*` but `now − file_mtime > 120 s` **and** `now − lastEventTime > 120 s`, render `working?`/`stale` — the process is probably gone and the `turn/end` was never flushed (see §6.A, "Turns without turn/end"). Do not silently show "working" forever.
- **Queued input**: if the last `agent/inbox/spliced` has non-empty `inserted` and no `turn/start`/`user/message` followed, a new turn will start shortly → show "working (queued)" rather than `done`.
- **Running background jobs**: `job/registered` without `job/settled` is a *hint* of ongoing work, not proof (jobs can be orphaned by a crash). Use it only as a secondary line ("2 background jobs").
- **Open compaction**: `compaction/start` without `compaction/end` + an open turn → still `working`; if no open turn → `stopped (interrupted compaction)`.
- **Goal continuation**: if a `goal/change` left phase `active` with `roundsStarted < maxGoalRounds`, a `done` window is transient — the goal driver starts the next turn automatically. Show `continuing` (goal) rather than `done`.

### 5.3 Why "waiting" is detectable at all

Because the agent loop is strictly sequential: while a tool body awaits, the loop is inside `step()`; `step/end` is not written until the body returns. So an open step + an unmatched `tool/call` is exactly "a tool is running". `ask_user_question` and the approval answerer are tool bodies / pre-execute gates, so their waits show up as an unmatched call or an unmatched `approval/asked`.

Caveat for **timed** `ask_user_question` (not the default in this build — the logged tool schema has no `timeout` parameter, so `mode:"legacy"`): the tool may return a durable `tool/result` with body `{"pending":true,"callId":"…","message":"No answer batch arrived before the timeout…"}` and the turn continues/ends. Then the wait is visible only as that payload plus a later `user/message` with `source.kind == "user-question-reply"` and `outcome:"answered"`. Detect both.

### 5.4 Context usage formula (see E for the full discussion)

```
prompt_tokens = inputTokens + cacheReadTokens + cacheWriteTokens      // cacheWrite defaults 0
ratio         = prompt_tokens / contextWindow
```
Take `prompt_tokens` from the **newest** record that carries usage, `contextWindow` from the newest `request/context`.

---

## 6. Answers A–H

### A. Turn lifecycle

**Reason kinds** (`TurnEndReasonMap`, §3.1): `completed`, `aborted` (with `reason.kind` ∈ `user | parent | disposed | hook{reason} | legacy`), `blocked`, `error` (with `error: LlmFailure` → `message`, `code`, optional `status`/`requestId`/…), `max-tokens`, plus the two synthetic kinds `interrupted` and `forked`.

**User interruption** is recorded as:

```jsonc
{"type":"turn/end","seq":437,"time":1794488925693,
 "data":{"turn":3,"reason":{"kind":"aborted","reason":{"kind":"user"}}}}
```

(source: the Stop button → `agent.cancel({kind:'user'})`; the cause is copied by `abortedCancelCause`, `dsh-agent-loop/lib/index.js:495-509`, so no stack/extra fields leak into the log). If content was streaming, an `assistant/message` with `"interrupted":true` precedes it. If calls were still pending, their synthetic `ABORTED_BEFORE_DISPATCH` results precede `step/end`.

**Failed turn**:

```jsonc
{"type":"turn/end","data":{"turn":2,
  "reason":{"kind":"error","error":{"message":"…","code":"PROVIDER_HTTP_ERROR","status":429}}}}
```
For a non-`LlmError` throw the error object is `{message, code}` with `code = ToolActivationError.code` or `"UNKNOWN"` (`:809-816`).

**Turns without `turn/end`**: yes, on a hard crash / kill / power loss, and this is documented as a known limitation:

> **Root shutdown persistence** — shutting down the root context with an active Agent can leave its final `turn/end` in memory but absent from the persisted log. — `dsh-agent-loop/README.md:216`

`turn/end` is in a `finally`, so under normal control flow (including abort and error) it is always attempted; only process death skips it. The next open detects it (`openTurnClosers`) and appends `{kind:'interrupted'}` plus any missing `tool/result` / `step/end` (`dsh-session/lib/types/repair.js:35-78`). Real evidence: 3 sessions in the corpus end inside a turn — they are exactly the 3 sessions that were live while I scanned; the stale orphan cases prove that an open turn can persist indefinitely.

### B. Waiting-for-human signals

**(i) `ask_user_question`.** The registered tool name in this version is exactly **`ask_user_question`** — both modes:

```js
ctx.tools.register(defineTool({ name: "ask_user_question", … }))   // legacy, dsh-tool-ask-user/lib/index.js:223-224
ctx.tools.register(defineTool({ name: "ask_user_question", … }))   // timed,  dsh-tool-ask-user/lib/index.js:66-67
```
Confirmed at runtime by the logged `request/header.data.header.tools[].name` (this session's header lists `ask_user_question`). There is **no** `question` tool in DSH; the one `tool/call` named `"question"` in the corpus came from an external agent plugin (`codingns4dsh` mirrors the external agent's own tools into the log) — do not hard-code it.

Question text **is** in `data.arguments` as a raw JSON **string** (all 1,318 `tool/call.arguments` in the corpus are strings, never objects):

```jsonc
{"type":"tool/call","seq":115,
 "data":{"turn":1,"step":6,"callId":"call_98af…","name":"question",
         "arguments":"{\"requestId\":\"0\",\"questions\":[{\"id\":\"job_description\",\"question\":\"…\",\"header\":\"岗位信息\",\"options\":[{\"label\":\"…\",\"description\":\"…\"}]}]}"}}
```

Schema of the argument object for `ask_user_question` (from the live header):

```ts
{ questions: Array<{
    id: string,                       // required, unique within the call
    question: string,                 // required
    header?: string,
    options?: Array<{ label: string; description?: string }>,
    multi_select?: boolean
  }> }
// timed mode additionally accepts: timeout?: number   (seconds; -1 = block forever; default 120)
```

Answer shape (tool result text): `{"answers":[{"id","selected":string[],"custom"?}]}` — empty `selected` + no `custom` = the user explicitly skipped that question.

**Is an unanswered `tool/call` sufficient to detect a pending question?** In the default legacy mode, yes: `ask_user_question` blocks until a UI answer, so `tool/call` without `tool/result` (inside an open turn) = waiting for the answer. Two refinements: (a) the call is logged *before* the tool body, so there is a tiny window where the call exists but the UI has not rendered — harmless; (b) in **timed** mode the tool returns a durable `{pending:true}` result while remaining answerable, so you must additionally treat that payload as "waiting" until a `user/message` with `source.kind=='user-question-reply'` arrives. The projection unit does exactly this (`dsh-user-questions/lib/projection.js`): `state:'open'` (call logged, no result) and `state:'continued'` (pending result or `TOOL_OUTCOME_UNKNOWN`).

**(ii) Approval prompts.**

```jsonc
{"type":"approval/asked","seq":353,
 "data":{"id":"65bebb06-…","toolName":"plugin_manager","callId":"call_00_GThs…",
         "reason":"escalate sandbox to danger-full-access: plugin_manager {\"action\":\"install_bundle\",…}"}}
{"type":"approval/decided","seq":354,"data":{"id":"65bebb06-…","outcome":"allowed-once"}}
```

Outcomes are exactly `allowed-once | rejected | cancelled | unavailable` (`dsh-user-approval/lib/index.js:30-35`; observed 3 × allowed-once, 1 × rejected). Every ask is followed by exactly one decision on the same synchronous path, **unless the process dies while waiting** — the service appends the pair around an awaited answerer and `request()` refuses to run outside an open turn precisely so a bare `asked` cannot be crash-tail garbage:

```js
if (!this.approvalStateOf(session).turnOpen) throw new Error("approval.request() outside an open turn: the approval/asked + approval/decided audit pair must be turn-enclosed …");
session.append("approval/asked", {id, toolName, callId?, reason?});
const outcome = await this.decide(req, session);
session.append("approval/decided", {id, outcome});
```

So: **pending approval = unmatched `approval/asked`** (and it implies an open turn). Ordering relative to tools: `tool/call` is written first, then the approval pair, then the tool body runs — so a pending approval also has an unmatched `tool/call` whose `name` equals `approval/asked.toolName`. This is how you distinguish it from a normal long-running tool.

**(iii) Other "waiting for the user" records.**

- **Plan review** — `exit_plan_mode` tool call (plugin `dsh-plan-mode`; writes `plan/mode {active}` and, on approval, a silent pending exit). A pending `exit_plan_mode` call = waiting for plan review. **Not mounted in this build**: the live tool list has no `exit_plan_mode` and there are 0 `plan/mode` events.
- **Action card** — `propose_action` (plugin `dsh-action-cards`) creates a card *after the turn ends*; nothing runs until the user acts. A `propose_action` call + successful result with no `plugin:action-cards/decision` follow-up = waiting, even though no turn is open. Also not mounted here (no `propose_action` in the tool list).
- **Goal rounds** — `goal/change` (projection in cache, `phase: active|paused|blocked|complete`). A `blocked` goal with a `blockedReason.code` for human input is a genuine "needs a human" state; an `active` goal means the session will continue autonomously. 0 in this corpus.
- **`wait_agent` / `agent_subagent {action:"read"}`** — waiting on a *subagent*, not a human: it looks like an unmatched tool call inside an open turn → `working`.
- **Timed question continued after turn end** — see B(i).
- There is no dedicated "user is typing" or "client connected" record.

**(iv) Is `tool/result` guaranteed for rejected/denied calls?** Yes. Every started call is committed, including `ABORTED_BEFORE_DISPATCH`, policy denials, validation failures and rejected escalations (`pipeline.js:196-235`; a denied escalation throws inside the body, producing a normal error result). After a crash the guarantee is restored by repair (`TOOL_OUTCOME_UNKNOWN` / `TOOL_NOT_STARTED`). Only a crash *between* `tool/call` and `tool/result` breaks it temporarily, and the log then simply ends with the unmatched call.

### C. Tool activity

Observed `tool/call.name` frequencies (31 logs, live): `pwsh` 347, `web_fetch` 328, `web_search` 136, `bash` 124, `read` 111, `edit` 103, `write` 50, `read_image` 47, `grep` 23, `skill` 13, `subagent` 9, `load_workspace_dependencies` 8, `job_output` 8, `send_message` 7, `present` 7, `glob` 5, `list_subagent_models` 4, `working_directory` 3, `todo_write` 3, `job_list` 2, `question` 1, `agent_subagent` 1, `list_agents` 1, `plugin_manager` 1.

Two important facts:

1. **The name space is open.** The tool list visible to the model in this session contains 42 schemas — a mix of core DSH tools and plugin-provided ones: `agent_subagent`, `agent_team`, `ask_user_question`, `codingns_credential`, `create_goal`, `edit`, `get_goal`, `glob`, `grep`, `interrupt_agent`, `job_kill`, `job_list`, `job_output`, `list_agents`, `list_subagent_models`, `load_workspace_dependencies`, `plugin_manager`, `present`, `pwsh`, `read`, `read_image`, `schedule_create`, `schedule_edit`, `schedule_list`, `schedule_view`, `send_message`, `skill`, `spawn_teammate`, `subagent`, `subagent_fork`, `team_task_create`, `team_task_get`, `team_task_list`, `team_task_update`, `todo_write`, `update_goal`, `wait_agent`, `web_fetch`, `web_search`, `workflow`, `working_directory`, `write`. External agent plugins add more (`bash`, `question`, `exec-*` call ids, …). Never whitelist.
2. **There is no "user-visible action" flag.** The log does not classify tools into actions; the UI does that from tool name + result (`present` → deliverable; `edit`/`write` → file change, with `tool/result.meta.diffs`). The only durable action *requests* are the action-card `propose_action` (not mounted) and the approval pair.

**Is there a separate "approval request" tool wrapper for sandbox escalation? No.** Escalation is a pair of ordinary arguments on the *existing* tool (`sandbox_permissions` + `justification`, `dsh-tool-pwsh/lib/index.js:491-500,631`; same for bash/edit/write). `tool/call.name` stays `pwsh`/`edit`/…; the approval is requested inside pre-execute (`dsh-tools/lib/pipeline.js:386-390` `waterfall('tools/pre-execute') → {kind:'ask'} → serviceAsk`), producing `approval/asked{toolName, callId, reason}`. So a reader sees: `tool/call(name=pwsh)` → `approval/asked(toolName=pwsh, callId=…)` → `approval/decided` → `tool/result`.

### D. Progress & todos

`todo/write` shape and statuses: see §3.7 — `{todos:[{content:string, status:'pending'|'in_progress'|'completed'}]}`; **no `cancelled`**.

- Emitted on **every** `todo_write` tool invocation (whole-list snapshot), not on every item change. 3 rows in the whole corpus.
- **Resets per turn at the projection level**: `turn/start` → `todos = null`. Log-only readers must therefore scope the fold to the current turn (last `todo/write` with `seq > last turn/start.seq`).
- No agent-reported percentage exists (§3.7). Progress must be derived: todo counts, `step` counter, `sessionStats` (`cache`), or `job` status. If you show a bar, label it "todos 2/5", not "40%".

### E. Context usage

Token usage appears in exactly three places:

1. `assistant/message.data.usage` — `TokenUsage` (`inputTokens`, `outputTokens`, `totalTokens?`, `cacheReadTokens?`, `cacheWriteTokens?`, `reasoningTokens?`). Present on native model messages; **absent** on plugin-mirrored messages (`source.plugin`, 139 rows) and on 7 rows with no usage chunk.
2. `assistant/attempt.data.stream[].chunk` where `chunk.type=='usage'` — the same `TokenUsage`, but external adapters (codex/aicodemirror) add `cacheHitRate`, **`contextWindow`**, **`contextTokens`**, **`contextUsageRatio`**.
3. `request/context.data.contextWindow` — the route's window (state record, §3.5). Values seen: 1048576 (deepseek-flash/v4-pro), 500000 (gpt-6.1-sol), 1000000 (gpt-6-astra, sonnet).

Semantics (`dsh-token-meter/lib/types/turn-usage.js:normalizeUsage`):

```js
const knownPrompt = safeSum([inputTokens,
  ...cacheReadTokens===undefined?[]:[cacheReadTokens],
  ...cacheWriteTokens===undefined?[]:[cacheWriteTokens]]);
const exactPrompt = totalTokens - outputTokens;      // must be >= knownPrompt
if (cacheReadTokens!==undefined && cacheWriteTokens!==undefined && exactPrompt!==knownPrompt) return undefined;
```

i.e. **`inputTokens` is only the uncached prompt part**; the full prompt is `inputTokens + cacheReadTokens + cacheWriteTokens`, and `totalTokens = prompt + outputTokens`. Confirmed arithmetically on real rows (`24210+891+4224 = 29325`; `847+596+30336 = 31779`) and by the adapter's own `contextTokens` (`847+30336 = 31183`, `contextUsageRatio = 31183/500000 = 0.062366`).

**Recommended disk-only formula** (most recent request of a session):

```
window = newest_seen( request/context ).contextWindow
usage  = newest_by_seq( assistant/message.usage  ∪  assistant/attempt last usage chunk )
used   = usage.inputTokens + (usage.cacheReadTokens ?? 0) + (usage.cacheWriteTokens ?? 0)
ratio  = used / window                       // clamp to 1.0
```

Which record to prefer: **`assistant/message.data.usage`** is the semantic "what this request cost" record and exists for every successful native call; prefer it. Fall back to the **last usage chunk of the newest `assistant/attempt`** when the newest message carries no usage (plugin-mirrored messages) or when the session's provider only writes attempts. If both exist at the same step they agree (verified: codingns attempt `input 24421, cacheRead 4096` vs native message `input 24421, cacheRead 4096`). Only if the adapter itself supplies `contextTokens`/`contextUsageRatio` may you use them directly; do not require them.

Three caveats:

- The last usage describes the **last completed request**. While a request is streaming there is no usage yet — show the previous value (that is what DSH's own UI does via `contextPressure.pressureTokens`).
- After a compaction the surface shrinks immediately but the next request's usage only lands with the next `assistant/message`; context "used" will look stale for a few seconds.
- `cacheReadTokens` = prompt tokens served from provider cache (cheap, but still occupy the context); `cacheWriteTokens` = tokens written to the provider cache (reported by some routes, e.g. deepseek and claude-code); `inputTokens` = uncached prompt tokens. For "how full is the window" all three count.

### F. Titles

`session/title` shape: §3.6. Relationship:

1. At the first human `user/message` with text, a **fallback** title is appended immediately (`{kind:'fallback'}`, first 5 whitespace words, ≤40 UTF-8 bytes, normalized one-line) — source: `dsh-session-title/lib/index.js:619-651`, config from `dsh-base/cordis.patch.yml`.
2. In parallel a `session/title-llm-request` audit row records the auxiliary LLM call; on success a second `session/title` with `{kind:'provider', provider:'session-title-first-prompt-llm', model:{provider,model}}` **rewrites** the title (this is the same record type, last-write-wins).
3. A **manual rename** appends `session/title {title, messageSeqs: [], source:{kind:'user'}}` (`:301-317`) and pins the title (no further automatic generation). There is a rename API/RPC (`dsh-api-session-controller/lib/types/commands.js:186-200`, error code `session/title-invalid`).
4. `title` is truncated to `maxTitleBytes = 80` UTF-8 bytes and sanitized to a single line.

**What matches the sidebar:** the sidebar/session-list value is the folded **last `session/title`** (the projection `sessionTitle`, cached as `rows.title.val` in `<sessionId>.json`). So: use the last `session/title.data.title`. If no title exists yet (blank session), show the first prompt excerpt or "New session" — DSH shows nothing (`rows.title.val: null`, `sessionListMetadata.blank: true`).

### G. File growth (implementation-critical)

1. **Framing**: concatenation of independent checksummed zstd frames; frame 0 = header line only; **one frame per durable append batch** (≈ per flush), so a frame usually holds several JSONL rows (§1.4–1.5). Not one frame per record.
2. **Batching**: 200 ms debounce inside the write handle, plus mandatory flushes at: before the model request, before a top-level tool body, at every `agent/pre-step` checkpoint, on close/dispose, on explicit `flush()`. So a frame boundary is a durability boundary, not an event boundary.
3. **Appended, never rewritten**: current-generation file is append-only; a failed write rolls back to the prior length; committed events are never rewritten.
4. **Whole-file rewrite** happens only for a **format migration** (v3→v4 etc.): a new `session.v4.jsonl.zstd` is published atomically and the source generation is retained byte-identical. Compaction/rotation/pruning **do not** rewrite the log (compaction only appends events).
5. **Trailing partial frame is possible** (crash during write) — see §8.2 for the exact handling; DSH itself recovers the complete records and truncates the torn bytes before its next batch.
6. **Truncation / move on delete/archive**: none. There is no deletion API ("Nothing deletes session files", `dsh-session-persistence-jsonl/README.md:167`); "delete" in the UI only adds the id to `workspace.json.global.archivedSessionIds` (6 archived ids exist on this machine, all with intact logs). Archiving a *running* session does cancel its turn (`agent.cancel({kind:'user'})` without `keepInbox`, `dsh-agent/lib/types/archive-admission.js:29-33`), which appends `turn/end{aborted,user}` + `agent/inbox/spliced{outcome:'canceled'}`.
7. **Session directory contents**: exactly one file, the log (§1.1). No `meta.json`, no `session.json`, no `.lock` (Windows uses a named semaphore). A reader that wants metadata faster than decompressing should read `storages/session_projcache/sessions/<id>.json` and `storages/workspace.json` (§7), not a per-session sidecar — there isn't one.

### H. Session identity

- **Main sessions**: id minted as `session-${randomUUID()}` (`dsh-api-session-controller/lib/index.js:421` `createMinted`). The header `id` is authoritative; the directory is `encodeSegment(id)` (identity for these ids).
- **Subagent children**: id = bare `randomUUID()` (`dsh-subagent/lib/index.js:1403`), directory = the bare uuid; header carries `parentSession`, `origin:'subagent'`, `delegationDepth = parent+1` (`:281,353-356`), and `isSeeded:false`; the child's **first event (`seq:0`) is `subagent/descriptor`**.
- **Forks**: id `session-${randomUUID()}`, `isSeeded:true`, **no** `parentSession`/`origin` (`dsh-api-session-controller/lib/index.js:786-800`), inherited prefix in `header` + `session/end-seed{inherited:true}`; inherited tail closed with `turn/end{kind:'forked'}`. They *are* independent sessions and appear in the sidebar.
- **Ignoring children**: filter on `header.origin === 'subagent'` (or `parentSession` present, or directory name lacking the `session-` prefix). That is exactly the set absent from `workspace.json.workspaces[].sessionIds` in the real data. Children are still useful as "2 subagents running", read from the parent's `subagent/catalog` events / cache.
- **Project slug**: `--<projectKey>--` (§1.3) from `header.cwd`; `_no-cwd` when the header has no `cwd`. The slug is lossy/truncated (251 chars) — always trust `header.cwd`.

---

## 7. Fast path: the projection cache (recommended, with freshness rules)

`~/.dsh/storages/session_projcache/sessions/<sessionId>.json` (one file per session, JSON, atomically replaced via temp-file + rename — readers never see a half-written file; briefly a `.<uuid>.tmp` sibling may exist, ignore dot-tmp files).

Shape (`version: 7` on this build):

```jsonc
{
  "version": 7,
  "record": {
    "identity": { "formatVersion": 4, "createdAt": 1794753207647,
                  "cwd": "E:\\myAIProjects\\zetrix-dsh-screen",
                  "isSeeded": false, "inheritedEventCount": 0 },
    "rows": {
      "turnBoundary":   { "ver": 2, "seq": 1146,
                          "val": { "openTurnStartSeq": 738, "lastStepStartSeq": 1142,
                                   "lastStepBoundary": {"kind":"start","seq":1142},
                                   "lastTurn": 3 } },
      "title":          { "ver": 1, "seq": 1146, "val": "将 zectrix-workbuddy-pet …" },
      "todos":          { "ver": 2, "seq": 1146, "val": null },
      "contextPressure":{ "ver": 5, "seq": 1146,
                          "val": { "surfaceTokens": 230378, "contextWindow": 1048576,
                                   "pressureTokens": 302666, "sampledSurfaceTokens": 229410 } },
      "tokenUsage":     { "ver": 2, "seq": 1146,
                          "val": { "totals": {"uncachedInputTokens":195928, "outputTokens":120845,
                                               "cacheReadTokens":22430720, "cacheWriteTokens":0},
                                   "last": {"turn":3,"step":50,
                                            "buckets":{"uncachedInputTokens":458,"outputTokens":1138,
                                                       "cacheReadTokens":302208,"cacheWriteTokens":0}} } },
      "permissions":    { "ver": 2, "seq": 1146,
                          "val": { "preset":"danger-full-access","sandbox":"danger-full-access",
                                   "approval":"never","seeded":false } },
      "sessionStats":   { "ver": 1, "seq": 1146,
                          "val": { "turns":3,"steps":123,"llmMs":672007,"toolMs":710934,
                                   "ttftMs":120709,"ttftSteps":124,"decodeMs":551298,
                                   "decodeTokens":120845,"lastTurn":3,"openStep":null,
                                   "pendingCalls":{"call_00_…":1794754666238} } },
      "userQuestions":  { "ver": 2, "seq": 1146,
                          "val": { "inheritedEventCount":0, "timed":false,
                                   "questions": { "active": [], "settled": [] } } },
      "inbox":          { "ver": 1, "seq": 1146, "val": { "next-turn": [], "next-step": [] } },
      "plan":           { "ver": 4, "seq": 1146, "val": { "active": false, … } },
      "goal":           { "ver": 6, "seq": 1146, "val": { "current": null, … } },
      "subagentCatalog":{ "ver": 6, "seq": 1146, "val": { "head": { "values": [ … ] } } },
      "turnOutline":    { "ver": 2, "seq": 1146, "val": { "turns": [ … ], "draft": "…" } }
    }
  }
}
```

Rules that make it safe (`dsh-session-projection-cache/README.md`):

- **The log leads, the cache follows.** A crash can leave the cache *behind* the log, never ahead. Every row carries `seq` = the event-seq watermark that row was folded to.
- Writes are **mandatory** at session creation, at **every `turn/end`**, and at disposal; between those points they are throttled to `writeEveryEvents: 200` / `writeIntervalMs: 5000` (`dsh-base/cordis.patch.yml`).
- `identity` must match the log header (`formatVersion`, `createdAt`, `cwd`, `isSeeded`) or the record must be ignored.
- The cache path is absent for a session the app has never checkpointed.
- Row values are versioned (`ver`) and evolve: e.g. `contextPressure` v5 always has `surfaceTokens`, and adds `contextWindow` / `pressureTokens` / `sampledSurfaceTokens` only after a provider usage anchor exists. Treat every sub-field as optional.

**Recommended hybrid reader**:

1. Read the cache (if present) for `title`, `contextPressure`, `todos`, `permissions`, `turnOutline`.
2. Take `cache_as_of = min(row.seq)` (or a specific row's `seq`).
3. Tail the log from `max(cache_as_of, last_read_offset)` and fold only the newer events for `openTurn/pendingApprovals/pendingQuestions/lastReason`.
4. If the log has no event with `seq > cache_as_of`, the cached `turnBoundary` is exact — this is the common case at rest (a `turn/end` forces a write).

If you implement only one path, implement the log fold; the cache is an optimization and a source of richer metadata (title, context window, todos) that would otherwise require a full parse.

---

## 8. Tailing recipe (Rust)

### 8.1 Discovery

```
%USERPROFILE%\.dsh\sessions\<projectKey>\<sessionDir>\session[.vN].jsonl.zstd
```
Pick the highest `N` present (regex `^session(?:\.v([1-9][0-9]*))?\.jsonl(\.zstd)?$`; no suffix = v0). Parse the first frame, require `type=="session"`, `version==N` (refuse `>4`). Ignore `origin=='subagent'` sessions for the main display.

### 8.2 Poll loop

```
stat(file) -> size
if size < offset                    -> file was truncated/replaced: reset (offset=0, state=fresh)
if size == offset                   -> no change; optionally mtime for staleness only
bytes = read(offset, size-offset)   -> at most a few hundred KB; reads are frame-aligned because offset always is
loop:
  if len(bytes) < 4 or bytes[0..4] != 28 B5 2F FD  -> stop (partial frame header); keep offset
  n = find_frame_compressed_size(bytes)            // 0 or error -> stop (partial)
  if n > len(bytes)                                -> stop (partial frame); keep offset
  out = zstd_decompress_single_frame(bytes[0..n])  // MUST verify frame complete (checksum / eof)
  for line in out.split('\n'): parse & apply (skip empty; first line of frame 0 is the header)
  offset += n; bytes = bytes[n..]
```

Key invariants, empirically verified with `zstandard` 0.23 on a real 1,069,134-byte log:

- Decoding from **any frame boundary** works; frames are independent (no dictionary, no cross-frame state). The header frame produced 215 bytes; the next produced 261; a batching frame produced 83,862 bytes.
- A **truncated** tail frame does **not** raise: `decompressobj().decompress(raw[last:len-3])` returned 193 bytes with `eof == false`. **Therefore: never consume the output of a frame unless the decoder reports a complete frame (eof / checksum OK).** The safest rule is "only `eof == true`"; the DSH-compatible refinement is "parse only complete `\n`-terminated lines from a torn frame, then hold the remainder" — DSH itself recovers exactly those records on restart.
- Zstd frame descriptors here have the **checksum flag set** and **no content size**, so "complete" means "checksum verified / frame ended", not "size matched the header".
- If the writer later truncates and rewrites the torn bytes, the file length can *shrink*; your `offset` may then be past EOF. Handle `size < offset` by resetting to the last known-good frame boundary (store it) and re-reading.

### 8.3 Ignore / cheap-skip

- `request/header.data.header.tools` (tens of KB), `assistant/message.data.stream`, `assistant/message.data.message.content`, `tool/result.data.message.content`, `session/title-llm-request.data.system/messages` are the big fields. A metadata-only reader should deserialize lazily or with a streaming JSON parser and skip these subtrees.
- Keep per-session state small: a `u32` turn, `u32` step, and two small maps.
- Full rescan cost is bounded: 31 sessions, 37.8 MB uncompressed total, 4.2 MB worst case. A periodic full re-fold (e.g. every 60 s for the visible session) is acceptable; incremental tailing is better.

### 8.4 Event application (pseudo-code)

```rust
match ev.type_ {
  "turn/start" => { st.open_turn = Some(d.turn); st.open_step = None; st.todos = None; }
  "turn/end"   => { if st.open_turn == Some(d.turn) { st.open_turn = None; st.open_step = None; }
                    st.last_reason = Some(d.reason); }
  "step/start" => { st.open_step = Some(d.step); }
  "step/end"   => { st.open_step = None; }
  "tool/call"  => { st.pending_calls.insert(d.callId, (d.turn, d.step, d.name)); }
  "tool/result"=> { st.pending_calls.remove(&d.message.toolCallId); }
  "approval/asked"   => { st.pending_approvals.insert(d.id); }
  "approval/decided" => { st.pending_approvals.remove(&d.id); }
  "todo/write" => { st.todos = Some(d.todos); }
  "session/title" => { st.title = Some(d.title); }       // last wins
  "request/context" => { st.context_window = d.contextWindow; }
  "assistant/message" => if let Some(u) = d.usage { st.usage = Some((ev.seq, u)); }
  "assistant/attempt" => if let Some(u) = last_usage_chunk(&d.stream) { st.usage = Some((ev.seq, u)); }
  "session" (header)  => { st.header = Some(h); }        // frame 0 only
  _ => {}
}
```

For `tool/result`, prefer `data.message.toolCallId`; fall back to `data.message.source.callId`. For question state also check `data.message.content[0].text` for `"pending":true` when `pending_calls` contains an `ask_user_question` id that then disappears.

### 8.5 Refresh cadence (e-ink specific)

- Poll the **active** session's `stat` every 1 s; poll `workspace.json` / the session list every 10–30 s. A session log grows only at flush points, so sub-second polling buys nothing.
- Redraw the panel only when the derived state tuple `(state, label, elapsed_bucket, context_pct_bucket)` changes — e-ink partial refresh is slow and the log can be quiet for minutes during a long tool call.
- Never decompress the whole file per poll: keep `offset` + fold state in memory, or (cold start) read the projection cache and tail from its watermark.
- `elapsed` for `waiting-*` must come from the event's own `time`, not from file mtime (the file may have been flushed after the ask).
- Treat `file_mtime` as "last durable write", never as "last activity": a 200 ms-debounced batch can land after a long silence, and a crash leaves the mtime frozen while the process is gone.

---

## 9. Forward/backward compatibility

- Version gates to enforce: header `version` must be `4` (or a supported older generation, which requires the migration chain — a read-only reader should **refuse**, not guess); event `type` unknown → skip; `ignorable:true` → skip; unknown fields inside known events → ignore.
- The v3→v4 migration package exists (`dsh-session-format-v3-to-v4`), so a machine upgraded in place may still hold `session.v3.jsonl.zstd` next to (or instead of) v4 files. Real machine: only v4 files.
- Same-version additions are allowed ("Established record meanings remain stable; compatibility review governs same-version additions", `dsh-session-format/README.md:44`), so treat every `data` field as potentially extended.
- `time` is not monotonic across sessions; compare only within a session (and only for staleness).

---

## 10. Appendix — corpus statistics (31 logs, 2026-11-15 scan)

> Counts are indicative: 3 sessions were live and growing while I scanned, so totals drift by a few dozen. The shape and the enum values are exact.

```
tool/call 1318   tool/result 1316   assistant/message 1015   step/start 879   step/end 876
session-log-deepseek/delivery-accepted 790   web/deepseek-search-llm-request 373
job/registered 333   job/settled 328   agent/inbox/spliced 283   user/message 270
assistant/attempt 159   turn/start 75   turn/end 72   session/title 49
session/title-llm-request 35   sandbox/mode 33   approval/policy 33   session 31
request/header 31   system/message 28   request/context 28   permission/preset 27
workspace/changes 18   model/selection 13   subagent/descriptor 9   subagent/catalog 9
session/end-seed 8   deliverables/presented 7   approval/asked 4   approval/decided 4
todo/write 3   command/run 2   command/done 2   compaction/start 1   compaction/summary 1
compaction/end 1   developer/message 1
plan/mode 0   goal/change 0   llm/retry 0   image/offload 0   team/* 0
turn/end reasons: completed 66, aborted{user} 6
approval outcomes: allowed-once 3, rejected 1
```

Validation results (all 31 files): `seq` contiguous from 0; `time` non-decreasing; exactly one header line per file; exactly one file per session directory.

## 11. Appendix — evidence index (decisive citations)

| Claim | Evidence |
|---|---|
| Event union / optionality | `dsh-api-session-controller/lib/typert.host.js` `"name":"SessionEventMap"`; same strings in `dsh-agent-preset-registry`, `dsh-action-cards` |
| turn/end kinds, cancel causes | `dsh-agent-loop/lib/index.js:495-509, 800-826`; `TurnEndReasonMap` declaration |
| interruption / crash closer | `dsh-session/lib/types/repair.js:35-78`; `dsh-agent-loop/README.md:216` |
| approval pair + outcomes | `dsh-user-approval/lib/index.js:30-35, 153-168` |
| ask_user_question name/schema | `dsh-tool-ask-user/lib/index.js:66-122, 223-273`; live `request/header` tools |
| tool call/result pairing | `dsh-tools/lib/pipeline.js:104-108, 185-235` |
| escalation reason | `dsh-sandbox/lib/index.js:147, 154-160`; `dsh-tool-pwsh/lib/index.js:631` |
| todo statuses + reset | `dsh-tool-todo/lib/index.js:14-18, 85-89, 173-176` |
| token semantics | `dsh-token-meter/lib/types/turn-usage.js` `normalizeUsage`; `dsh-token-meter/README.md` ("Reading the numbers") |
| title rename / fallback / provider | `dsh-session-title/lib/index.js:301-317, 480-490, 619-651`; `dsh-base/cordis.patch.yml` |
| framing / batching / durability | `dsh-session-persistence-jsonl/README.md:77, 111`; `lib/index.js:187-194` |
| filename scheme | `dsh-session-format/lib/index.js:464-489` |
| slug/session dir encoding | `dsh-session-persistence-jsonl/lib/index.js:858-899` |
| projection cache | `dsh-session-projection-cache/README.md`; real `<id>.json` files |
| workspace registry / archive | `~/.dsh/storages/workspace.json`; `dsh-agent/lib/types/archive-admission.js` |
| compaction lifecycle | `dsh-compaction-basic/lib/index.js:441-520`; `dsh-compaction/lib/types/checkpoint.js` |
| jobs | `dsh-jobs-local/lib/index.js:604-624`; `JobRegisteredData`/`JobSettledData` |
| session ids | `dsh-api-session-controller/lib/index.js:421, 786-800`; `dsh-subagent/lib/index.js:353-356` |
| commands | `dsh-commands/lib/index.js:334-380` |
| end-seed | `dsh-session/lib/index.js:1423-1430` |
| delivery-accepted | `dsh-session-log-deepseek/README.md:66`; `lib/index.js:190-196` |
| workspace/changes pointer | `dsh-workspace-changes/lib/types/recorder.js:282` |
