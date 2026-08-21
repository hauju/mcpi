# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

A native MCP inspector — connect to MCP servers, browse tools/resources/prompts, call them from
generated forms, replay calls from history, and diff capability schemas across time.

The thing that must never silently regress is **classifying a contract change across elapsed
time**. Everything else here — history, replay, collections, generated forms — is convenience
layered on top of the local store that makes the diff possible. A local store is the *precondition*
for the diff, not the point of the project: `@modelcontextprotocol/inspector` 2.x persists a
writable server catalog and OAuth tokens too, and so do the free forks. Do not write copy, or make
a scope decision, that assumes otherwise.

Two claims to keep honest:

- **"Native" means a Rust binary, not AppKit.** Dioxus desktop drives a system WebView (wry/tao).
  The defensible pitch is "a Rust binary, no Node, no Electron" — never "native UI".
- **Diagnosis is the door; the diff is the bet.** Connection and auth failure is the loud, common
  pain, but the official inspector answers much of it now — Connection Info, mid-session re-auth,
  a `stderr` console, a JSON-RPC transcript. So `probe` has to be *better* than that, not merely
  present. When trading off scope, keep the probe's report truthful and the diff engine's
  classification correct, in that order, both before anything else.

## Layout

```
├── apps/
│   ├── mcpi/        Dioxus 0.7 DESKTOP app — the product
│   └── mcpi-cli/    CI companion: snapshot + lint + diff, exit 1 on breaking
└── crates/
    ├── schemadiff/  Snapshot → classified diff. Pure logic, no I/O, no rmcp.
    ├── mcplint/     Snapshot → static spec-conformance facts. Pure logic, no I/O, no rmcp.
    ├── probe/       Diagnoses an endpoint from outside: plain HTTP, no session, no credentials
    ├── mcpclient/   rmcp 3.x client wrapper → cloneable `Handle`
    ├── mcpstore/    SQLite: servers, snapshots, call history, collections
    └── mockserver/  Hermetic stdio MCP server fixture, two schema variants
```

`crypto` and `auth` come from [dx-kit](https://github.com/hauju/dx-kit) where needed; do not
vendor a local copy.

This is a **virtual workspace** — no root package.

`schemadiff`, `mcplint`, `probe`, and `mcpclient` are also consumed by a separate private site
repo as git dependencies pinned by tag. A breaking change to any of their public APIs is a
cross-repo break: bump the tag deliberately rather than assuming this repo is the only consumer.

## Commands

```sh
just app          # dx serve --package mcpi --platform desktop
just check        # fmt + clippy + tests
just test         # tests alone
just tw           # rebuild the app's Tailwind by hand
```

## How the crates fit together

```
mockserver ──stdio──▶ mcpclient ──Snapshot──▶ mcpstore ──diff──▶ schemadiff
   fixture             Handle                  SQLite            Severity
```

- **`schemadiff`** is pure logic with no I/O and no `rmcp` dependency. A `Snapshot` holds raw
  `serde_json::Value`s keyed by name, not typed MCP structs, so a field added by a future spec
  revision still shows up in a diff instead of being dropped on deserialize. `diff()` classifies
  every change; the classification, not the diff, is the product. Rules are parameterised by
  direction — the same edit to an `inputSchema` and an `outputSchema` means opposite things,
  because one is data the caller sends and the other is data it receives.
- **`mcpclient`** wraps `rmcp` behind `Handle`: cloneable, `Send + Sync`, backed by a task that
  owns the non-`Clone` `RunningService`. Commands are dispatched concurrently onto a cloned peer so
  a slow tool call cannot block a list refresh. Server notifications fan out on a broadcast channel
  from the moment a session connects — a handler cannot be retrofitted onto a live session, and a
  dropped notification cannot be recovered.
- **`mcpstore`** owns persistence. `record_snapshot` hashes the canonical form, writes a row only
  when it differs from the server's previous snapshot, and returns `First` / `Unchanged` /
  `Changed { diff }`. That single call is what the connect path uses to decide whether to badge the
  UI.
- **`mockserver`** is a hermetic stdio fixture with two variants whose contracts differ across
  every severity class. `cargo test -p mcpclient` connects to both for real and asserts on the
  resulting diff, so the client, the store's digest, and the classifier are verified together
  without a network or Node.

Snapshot digests are blake3 over `canonical_json`, which sorts object keys at every level. Do not
switch it to `serde_json::to_vec` — `serde_json`'s map type becomes order-preserving if anything in
the graph enables `preserve_order`, and every digest would silently change, making every server
look like it had moved.

## The app (`apps/mcpi`)

Three panes: `Sidebar` (server library) → `Browser` (what the connected server advertises) →
`Detail` (the selected item). `ServerDialog` is a modal over the top. All four read `AppState` from
context; none of them take it as a prop.

- **`AppState` is `Copy`.** The store lives in a `Signal<Arc<Store>>` rather than a bare `Arc` field
  precisely so the whole struct stays `Copy` — one non-`Copy` field would force `let app =
  app.clone()` at every event handler and every `spawn`. Read it through `AppState::store()`, which
  clones the `Arc` out immediately; never hold a signal borrow across an `await`.
- **`Conn` cannot be a prop.** It holds a live `Handle`, which has no meaningful `PartialEq`, and
  Dioxus props require it. Pass `Conn::status()` (a `Copy` enum) instead.
- **Connecting is one action, in `AppState::connect`.** It dials, snapshots, records, and stores the
  resulting diff in `Conn::Connected`. Recording is what turns "connected" into "connected, and here
  is what changed" — keep those together rather than making the UI orchestrate them.
- **The dialog edits text, not structure.** Arguments, environment, and headers are textareas parsed
  on save (`config::lines` / `config::pairs`), because people configure MCP servers by pasting out
  of a JSON config file and a textarea survives a paste where a row editor does not. Arguments are
  split per line, not on whitespace, so a path with a space in it stays one argument.

### The call form (`form.rs`, `components/call_form.rs`)

`form.rs` maps a JSON Schema onto a `Widget` and back. It covers a deliberate subset, and anything
it cannot represent falls back to a raw JSON editor **for that field alone**, carrying a visible
`raw` badge. A form that silently mangles a payload it did not understand is worse than one that
admits the limit.

**The subset is defined by what real generators emit, not by what the spec allows.** Every MCP
server is built with schemars (Rust), pydantic (Python), or zod (TypeScript), and each spells
"optional string" differently. Getting this wrong is not a cosmetic miss — it drops most arguments
on most servers into textareas, which is the form feature failing at its job:

| Idiom | Emitted by | Handling |
|---|---|---|
| `{"type": ["string","null"]}` | schemars, zod 3 `.nullable()` | the concrete type, nullable |
| `{"anyOf":[{...},{"type":"null"}]}` | pydantic, zod 4 `.nullable()` | null branch stripped, then the remainder |
| `{"$ref": "#/$defs/X"}` | schemars, pydantic (structs, enums) | resolved against the root's `$defs` |
| `allOf` with one element | pydantic 2.0–2.8, when a field has a description | unwrapped |
| `oneOf`/`anyOf` of `const` branches | schemars documented enums, zod 4 literal unions | a `Choice` select |

Still raw, correctly: unions of two or more real types, multi-schema `allOf`, `not`, non-local
`$ref`, and lists of structured values. Each genuinely cannot map to one widget.

- **Resolution is internal.** `properties_of` / `widget_for` keep their one-arg signatures — the
  schema they are handed *is* the root — and thread the root plus a resolving chain through private
  helpers. A self-referential schema stops at the cycle rather than recursing forever.
- **Composition is checked before `type`.** A schema can carry both; honouring the `type` would drop
  the constraint the composition expresses. The exception is the nullable idioms above, where the
  composition *is* the nullability.
- **Seeding fills required fields only.** Optional fields stay absent so the server applies its own
  defaults rather than receiving an empty value nobody asked for. A `"default": null` is skipped for
  the same reason — pydantic stamps one onto every optional field, and honouring it would put an
  explicit `null` in every payload. A *required* nullable field seeds to `null`, which the tool
  declared it accepts, rather than a fabricated `""` or `0`.
- **Objects are expanded in `PropertyField`, not `WidgetInput`.** Only that scope knows the parent
  path to prefix onto each child; expanding them a level down writes every nested field to the top
  of the payload instead. The `Widget::Object` arm inside `WidgetInput` is a defensive fallback, not
  the normal path.
- **A number that will not parse is sent as the typed string.** Substituting a zero would send a
  value the user never entered; the server's type error is the more useful answer.
- **The raw JSON editor keeps a local buffer.** It re-syncs only when the payload moves underneath
  it (loading from history), so a half-typed edit is not destroyed on every keystroke.

### The diff surface (`components/diff.rs`)

The headline feature, shown in four places that all read one source — `Connected::status`:

| Where | What it shows |
|---|---|
| Sidebar row | count of breaking changes, as a red badge |
| `DiffBanner` above the item list | the summary line, and the way into the drawer |
| Per-item markers in the browser | `breaking` / `changed` |
| `ItemChanges` in the detail pane | the selected item's field changes, inline |
| `DiffDrawer` | everything, grouped by tools / resources / prompts / capabilities |

- **`ContractStatus` has three states, not two.** `First` and `Unchanged` are different answers —
  collapsing both into "no diff" would make a first connect look like a clean bill of health.
- **`$defs` are diffed, not just referenced.** A schemars server can gut an enum behind a `$ref`
  that itself never changes; walking only properties produced an *empty* diff for a real breaking
  change — silence, which is worse than a false alarm. A removed definition that something still
  references is breaking; an unreferenced one is cosmetic, because refactor cleanup must not cry
  wolf.
- **The nullable wrapper is unwrapped before the opaque-composition rule.** Otherwise every inner
  edit to an optional field on a pydantic server — even a loosened enum, which is genuinely
  compatible — reported as unanalysable and therefore breaking.
- **Instruction rewrites are named apart from severity.** `SnapshotDiff::instruction_rewrites`
  reports items whose `description` or `annotations` changed. A rewritten description stays
  `Cosmetic` and that is correct — no caller's code breaks — but the description *is* the
  instruction an agent follows and `annotations` is what it consents on, so a silent edit is the
  shape a rug pull takes. Severity answers "will my code still work"; this answers "did the
  instructions move". Collapsing them makes one of the two unanswerable. Two snapshots cannot show
  whether an edit was innocent, so every consumer states what changed and never that a server is
  malicious.
- **Judgement before evidence.** Every surface leads with the classification ("1 breaking") and puts
  the JSON second. A wall of before/after payloads is what someone squinting at two terminal windows
  already has.
- **`ItemChanges` sits above the call form.** Learning a tool just broke changes whether you want to
  call it at all.
- **Severity is the only thing that gets colour**, here and in `tailwind.css`. Red/amber never mean
  UI state anywhere in the app, so they always mean the contract moved.
- Additions show only an `after` line and removals only a `before` one; printing an empty
  counterpart reads as "changed to nothing".
- **Markdown export lives in `schemadiff::to_markdown`, not the app.** The drawer's copy buttons and
  the CLI must emit the identical artifact; the renderer follows the same rules as the drawer
  (judgement first, one-sided lines for additions and removals).
- **A baseline is a labelled snapshot**, not its own table — a `label` column, unique per server via
  a partial index. `AppState.baselines` is kept separate from `AppState.snapshots`: a pin can be
  older than the timeline window, and splicing it into the list would fabricate a "consecutive" pair
  that skips real changes. `Store::labeled_snapshots` is deliberately unlimited so a pin outlives
  the window — that reachability is the point of naming one.

### The CLI (`apps/mcpi-cli`)

The free, CI-shaped surface over the same engine — the open-differ-first move every neighbouring
ecosystem's winner made (Buf, Optic, oasdiff). `snapshot` prints a contract as JSON; `lint` checks
it against the spec's static tool rules; `diff` classifies two contracts. Exit `1` on a breaking
change or a violated MUST, `2` on operational failure, so a pipeline can gate on it directly.

- **A source string is one of four forms**: a snapshot file, `http(s)://` (dialled live),
  `stdio:command args` (spawned live), or `@label` — a baseline pinned in the desktop app, read from
  the same `store.db` via `mcpstore::default_path`. Auth is `--header` only; the CLI never opens a
  browser and never touches the keychain.
- **The binary is `mcpi-cli`, not `mcpi`** — the desktop app already claims that output filename in
  the shared target directory, and two workspace binaries with one name collide. Revisit only at
  standalone release time.
- Markdown output comes from `schemadiff::to_markdown`, so the CLI and the drawer's export button
  emit the identical artifact.
- `tests/cli.rs` spawns the real binary against the real mockserver fixture and asserts on exit
  codes — the two things a pipeline actually consumes.

### Collections (`components/collections.rs`)

A saved sequence of calls — the handful you make every time you touch a server, fired at a new build
in one click. Steps are stored as `(kind, target, request)`, which is a `NewCall` minus its outcome.

- **Runs are sequential, and do not stop at the first failure.** Later steps routinely depend on
  earlier ones, so concurrency would make results depend on scheduling; and a smoke test's job is to
  report everything that broke, not the earliest thing.
- **Every step lands in call history**, so it stays openable and replayable on its own rather than
  vanishing into a batch.
- **Saving captures the form, not the last result.** You build a smoke test out of the calls you
  meant to make, not the ones that happened to succeed.
- **`ord` is sparse.** Deleting a step leaves a gap rather than renumbering; ordering only ever
  compares, and appending takes `MAX(ord) + 1`.
- Only failed steps expand their output. A passing run should be one glance.

### Telling someone their server is wrong (`mcplint`, `probe`, `conformance.rs`, `auth_hint.rs`)

Three crates answer "is this server built correctly", split by what each can see, and **all three
state facts and refuse to grade** — these findings appear next to other people's servers, and a
letter grade starts a fight where "4 tools return no annotations" gets fixed.

| Crate | Sees | Answers |
|---|---|---|
| `mcplint` | a `Snapshot`, no I/O | static conformance: schema shape, naming, descriptions |
| `probe` | plain HTTP, no session, no credentials | why it will not connect; what it offers anonymously |
| `schemadiff` | two snapshots | what moved, and how badly |

- **`mcplint` has no score or grade, on purpose** — its own module docs say so. The app's
  `Conformance` renders a *list* and never "3 warnings"; the CLI prints a finding count the way
  clippy does, which counts findings rather than rating the server. `Level` says only whether the
  spec word is MUST or SHOULD; it describes the spec, not the server, so it gets a text weight and
  never a colour. Red and amber mean the contract moved, everywhere in this app.
- **Conformance findings render above the call form**, for the same reason `ItemChanges` does: a
  violated MUST gets the tool dropped by a conforming client, which is worth knowing before you try
  to call it. Server-level findings (`tool: None`) go in `ServerSummary` — hence `Conformance`'s
  `bare` prop, since that column already has padding.
- **`declares_auth` lives in `probe`, and the app delegates to it.** Two copies of the heuristic
  would drift, and then the app would lock a tool the endpoint report calls open — with neither
  surface wrong on its own.

**The auth lock has two states because it has two signals, and they are not equally good.** MCP has
no "requires auth" field, so:

- **Observed** (solid lock, `auth` label): the tool's *most recent* call came back an auth error.
  `Store::last_call_failures` returns only the latest call per tool, so the lock clears itself once
  the call succeeds — no expiry logic, and no mark left behind after you add a key. Read from the
  whole call log, not the loaded `history` window, because a lock earned fifty calls ago is exactly
  the one still worth showing.
- **Declared** (hollow lock): only the tool's own prose demands credentials. Checked against the
  **full** description, not `summary_of`'s first sentence — servers put the requirement last, after
  they have finished selling the tool.

Guessing and knowing must stay legible as different things, or the mark stops meaning anything.
Observed beats declared when both apply.

`probe` reports the same problem from outside as a `Warning`: a server that initializes
anonymously, lists tools whose descriptions demand credentials, and issues no `WWW-Authenticate`
challenge has put its auth in prose instead of in the protocol — so a call reaches the tool and
returns a tool error instead of an authorization step, and no client can offer a sign-in button.

### The wire transcript (`mcpclient/src/wire.rs`)

The frames are the only account of a session that is not an interpretation of one. Wrapping the
*transport* rather than the peer is what makes the transcript complete: `initialize` and the
handshake happen before any `Handle` exists, and a notification the client does not model still
shows up as bytes.

Nothing is masked, deliberately. This layer only ever sees JSON-RPC bodies — OAuth tokens and
custom headers ride the HTTP layer above it and never reach a frame — so there is no secret here to
redact, and a redaction that hid a real argument would defeat the point of having the transcript.

## Cross-cutting constraints

- **Never run `cargo clippy`/`test`/`check` with `--workspace`.** One invocation unifies features
  across members, which would resolve `dioxus/desktop` into the headless crates' graph. Every recipe
  in the `justfile` and every CI job selects packages explicitly with `-p`; keep it that way when
  adding members.

- **`rusqlite` is pinned to 0.32.** This pin originated in a monorepo whose single lockfile also
  held the site's `sqlx-sqlite`: both declare `links = "sqlite3"`, and cargo enforces uniqueness of
  that value across the whole lockfile. That constraint no longer applies in this repo — the site
  lives elsewhere with its own lockfile — so the pin is bumpable in principle. It has not been
  tested; if you bump it, run the full `mcpstore` suite.

- **Secrets never go in SQLite.** API keys and OAuth tokens live in the macOS Keychain via the
  `keyring` crate, keyed by server id. `mcpstore` persists only a marker that a secret exists.

- **macOS GUI apps do not inherit your shell `PATH`.** Launched from Finder, a bundled app gets
  `/usr/bin:/bin:/usr/sbin:/sbin`, so `npx`, `uvx`, and `bun` do not resolve and every stdio MCP
  server fails to spawn. `mcpclient` resolves the real `PATH` once via `$SHELL -ilc 'echo $PATH'`
  and injects it into every spawned child. This is the single most common MCP configuration
  failure — do not "simplify" it away, and keep the resolved value visible in Settings so a failed
  spawn is diagnosable.

- **rmcp 3.x has no legacy HTTP+SSE client transport.** Only `TokioChildProcess` (stdio),
  `StreamableHttpClientTransport`, and a Unix-socket client are exported. Servers speaking the
  deprecated 2024-11-05 `/sse` + `POST /messages` transport cannot connect. When that fails, the
  error must name the cause rather than surfacing a generic connection error.

## OAuth (`crates/mcpclient/src/oauth.rs`)

Every remote connection is wrapped in `AuthClient`, whether or not the server wants OAuth. It sends
the first request unauthenticated when it holds no credentials, so a server needing none is
unaffected, and one that does answers `401` with a challenge. That gives one code path plus free
token refresh on a rejected token.

- **`ClientInitializeError::TransportError` breaks the error chain.** Its `DynamicTransportError`
  sits in a field named `error` with no `#[source]` attribute, so `source()` returns `None` there
  and a plain walk stops one level above everything interesting. `session::auth_challenge` matches
  that variant explicitly to step across the gap. Covered by `tests/http_auth.rs` — if that test
  starts failing after an rmcp upgrade, the symptom in the app is the sign-in button disappearing
  in favour of a generic error.
- **Credentials go to the keychain, keyed by row id** (`config::credential_key`), not by URL: two
  saved servers can point at the same deployment and have been authorized as different people.
- **Loopback redirect on an ephemeral port**, per RFC 8252 §7.3, which requires authorization
  servers to accept any port for loopback.
- **The callback listener answers unrelated requests and keeps waiting.** Browsers fetch
  `/favicon.ico` against the redirect host unprompted; aborting sign-in over that would make the
  flow fail at random.
- Keychain calls run on `spawn_blocking` — they block and can put a system prompt on screen.

---

## Dioxus 0.7 Reference

You are an expert [0.7 Dioxus](https://dioxuslabs.com/learn/0.7) assistant. Dioxus 0.7 changes every api in dioxus. Only use this up to date documentation. `cx`, `Scope`, and `use_state` are gone.

### Launching

```rust
use dioxus::prelude::*;

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! { "Hello, Dioxus!" }
}
```

### RSX

```rust
rsx! {
    div {
        class: "container",
        color: "red",
        width: if condition { "100%" },
        "Hello, Dioxus!"
    }
    for i in 0..5 {
        div { "{i}" }
    }
    if condition {
        div { "Condition is true!" }
    }
    {children}
    {(0..5).map(|i| rsx! { span { "Item {i}" } })}
}
```

### Assets

```rust
rsx! {
    img { src: asset!("/assets/image.png"), alt: "An image" }
    document::Stylesheet { href: asset!("/assets/styles.css") }
}
```

### Components & Props

- Annotate with `#[component]`, function name starts with capital letter.
- Props must be owned (`String` not `&str`), implement `PartialEq` + `Clone`.
- Wrap in `ReadOnlySignal` for reactive props.
- Re-renders when props change or internal reactive state updates.

### State

```rust
// Local state
let mut count = use_signal(|| 0);
let doubled = use_memo(move || count() * 2);

// Read: count() clones, count.read() borrows
// Write: *count.write() += 1  or  count.with_mut(|c| *c += 1)

// Context API
use_context_provider(|| Signal::new(value));
let ctx = use_context::<Signal<T>>();
```

### Async

```rust
let data = use_resource(move || async move { fetch().await });
match data() {
    Some(value) => rsx! { "{value}" },
    None => rsx! { "Loading..." },
}
```

### Styling

TailwindCSS 4 + DaisyUI 5. Dioxus 0.7+ auto-detects a `tailwind.css` next to the package manifest
and runs Tailwind during `dx serve`. Icons via `dioxus-free-icons` with the Lucide set.
