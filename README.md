# mcpi

A native MCP inspector that remembers what your server's contract used to look like — and tells you
what broke.

Connect to stdio and remote MCP servers, browse their tools, resources and prompts, call them from
forms generated off their JSON schemas, replay anything from history, and diff a server's contract
against any point in its past with every change classified as **breaking**, **compatible**, or
**cosmetic**.

Rust, top to bottom. No Node, no Electron.

## What this is actually for

Two things here are hard to get elsewhere, and it is worth being precise about which:

**Classifying a contract change across elapsed time.** Not "here are two JSON blobs, spot the
difference" — an actual judgement about whether the edit breaks your callers, with the rules
parameterised by direction, because the same change to an `inputSchema` and an `outputSchema` means
opposite things. One is data you send; the other is data you receive.

**A single Rust binary.** The CLI drops into CI with no Node toolchain and exits `1` on a breaking
change.

Everything else — saved servers, persisted auth, call history, replay, collections, generated
forms — is convenience layered on top. It is genuinely nice, and it is genuinely not unique:
`@modelcontextprotocol/inspector` 2.x persists a writable server catalog and OAuth tokens, and the
free forks do too. A local store is the *precondition* for the diff, not a reason to pick this over
the official tool. If all you need is to poke a server once, `npx @modelcontextprotocol/inspector`
is right there and it is good.

## The diff

Every surface leads with the classification and puts the JSON second, because a wall of
before/after payloads is what you already have from two terminal windows.

```console
$ mcpi-cli diff baseline.json stdio:./mockserver --variant b
# Contract changes

baseline.json → ./mockserver --variant b

**4 breaking** · 1 compatible

## Tools

### `deprecated_tool` · removed · breaking

### `search` · changed · breaking

- cosmetic `description` — description changed
  - before: `"Search the index."`
  - after: `"Search the index. Now with pagination."`
- **breaking** `inputSchema.properties.limit.maximum` — `maximum` tightened
  - before: `100`
  - after: `10`
- **breaking** `inputSchema.properties.mode.enum` — values removed from the list
  - before: `["fast","slow","auto"]`
  - after: `["fast","slow"]`
- compatible `inputSchema.properties.cursor` — optional property added
  - after: `{"description":"Opaque page cursor","type":"string"}`

## Resources

### `mock://notes` · changed · breaking

- **breaking** `mimeType` — MIME type changed — consumers parse the body based on this
  - before: `"text/markdown"`
  - after: `"text/html"`

$ echo $?
1
```

It is Markdown because that is what you paste into the PR that caused it. `--format json` gives you
the raw `SnapshotDiff` for `jq`.

A few things it gets right that a structural differ does not:

- **`$defs` are diffed, not just referenced.** A schemars server can gut an enum behind a `$ref`
  that itself never changes. Walking only properties produces an *empty* diff for a real breaking
  change — silence, which is worse than a false alarm.
- **Nullable wrappers are unwrapped first.** Otherwise every edit to an optional field on a pydantic
  server — including a *loosened* enum, which is genuinely compatible — reports as unanalysable and
  therefore breaking.
- **A first connect is not a clean bill of health.** `First` and `Unchanged` are different answers
  and are reported as such.
- **Instruction rewrites are named separately from severity.** A rewritten tool description is
  cosmetic — no caller's code breaks — but the description *is* the instruction an agent follows, so
  a silent edit to it is the shape a rug pull takes. Severity answers "will my code still work".
  That answers "did the instructions move". Neither question can be folded into the other.

## The CLI

```sh
mcpi-cli snapshot https://api.example.com/mcp > baseline.json
mcpi-cli diff baseline.json stdio:npx -y @modelcontextprotocol/server-filesystem /tmp
mcpi-cli lint https://api.example.com/mcp
```

A source is one of four forms: a snapshot file, an `http(s)://` URL (dialled live), `stdio:command
args` (spawned live), or `@label` — a baseline pinned in the desktop app. Exit codes are `1` for a
breaking change or a violated MUST, `2` for operational failure, so a pipeline can gate on it
directly.

Auth is `--header` only. The CLI never opens a browser and never touches your keychain.

### In GitHub Actions

[`hauju/mcpi-action`](https://github.com/hauju/mcpi-action) wraps the CLI: it installs a prebuilt
binary in seconds, fails the job on a breaking change or violated MUST, and posts the classified
diff as a PR comment.

```yaml
- uses: hauju/mcpi-action@v1
  with:
    source: "stdio:node dist/server.js"
    baseline: contracts/mcp-snapshot.json
```

## Conformance

`mcpi-cli lint` and the app's conformance pane report static spec facts about a server: schema
shape, naming, missing descriptions and annotations.

```console
$ mcpi-cli lint https://api.example.com/mcp
# Lint: https://api.example.com/mcp

Spec 2026-07-28 · 0 warnings, 4 notes

## search
- note · property-descriptions-missing — 2 of 3 argument properties have no description: limit, mode  [tools §inputSchema]

## server
- note · tool-annotations-absent — 3 of 3 tools declare no annotations (readOnlyHint, destructiveHint, idempotentHint, openWorldHint)  [tools §ToolAnnotations]
```

**There is deliberately no score or grade.** Every finding is `(rule, citation, fact)`, and the
level says only whether the spec word behind it is MUST or SHOULD — which describes the spec, not
your server. "4 tools return no annotations" is a thing someone fixes. "Quality: C−" is a thing
someone argues with. That matters because these findings are meant to be readable next to servers
you do not own.

## Status

Pre-release. No published binaries yet — build from source:

```sh
cargo install --path apps/mcpi-cli          # the CLI
dx serve --package mcpi --platform desktop  # the desktop app (needs `cargo binstall dioxus-cli`)
```

Requires Rust 1.97.1 (pinned in `rust-toolchain.toml`) and, for the app's stylesheet, `bun install`.

The desktop app drives the system WebView via wry/tao. "Native" here means a Rust binary with no
Node and no bundled browser — not AppKit widgets.

## Layout

| Path | What |
|------|------|
| `apps/mcpi` | The desktop app (Dioxus 0.7) |
| `apps/mcpi-cli` | CI companion — snapshot, lint, diff; exit 1 on breaking |
| `crates/schemadiff` | Snapshot → classified diff. Pure logic, no I/O, no rmcp |
| `crates/mcplint` | Snapshot → static spec-conformance facts. Pure logic |
| `crates/probe` | Diagnoses an endpoint from outside: plain HTTP, no session, no credentials |
| `crates/mcpclient` | `rmcp` client wrapper → cloneable `Handle` |
| `crates/mcpstore` | SQLite: servers, snapshots, call history, collections |
| `crates/mockserver` | Hermetic stdio MCP server fixture, two schema variants |

`schemadiff` and `mcplint` hold `serde_json::Value`s keyed by name rather than typed MCP structs, so
a field added by a future spec revision still shows up in a diff instead of being dropped on
deserialize.

## Development

```sh
just check   # fmt + clippy + tests
just test    # tests alone
just app     # dx serve --package mcpi --platform desktop
```

`cargo test -p mcpclient` connects to the real mockserver fixture over stdio and asserts on the
resulting diff, so the client, the store's digest, and the classifier are verified together without
a network or Node.

Never run cargo with `--workspace`; every recipe selects packages explicitly with `-p`.

## Contributing

Issues and PRs welcome. The one thing to know before touching `schemadiff`: **a
misclassification is the worst bug this project can have.** A false "compatible" lets a break
through; a false "breaking" trains people to ignore the tool. Changes to the rules need a test in
`crates/schemadiff/src/tests.rs` covering both directions.

## Licence

MIT. See [LICENSE](LICENSE).
