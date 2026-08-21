# The app reads CLIENT_METADATA_URL at *compile* time via option_env!, which
# only works if the build itself runs with it set. A missing .env is fine — the
# app then registers its OAuth client dynamically instead.
set dotenv-load := true

clean:
    cargo clean

fmt:
    cargo fmt --all -- --check

# Clippy runs per-package rather than with --workspace: one invocation unifies
# features across members, and `dioxus/desktop` should not leak into the
# headless crates' graph.
clippy:
    cargo clippy -p schemadiff -p mcplint -p mcpclient -p mcpstore -p mockserver -p probe -p mcpi-cli --all-targets -- -D warnings
    cargo clippy -p mcpi --all-targets -- -D warnings

# mcpclient's and mcpi-cli's suites spawn the mockserver binary over stdio, so
# it has to exist first.
test:
    cargo build -p mockserver
    cargo test -p schemadiff -p mcplint -p mcpclient -p mcpstore -p probe -p mcpi -p mcpi-cli

check: fmt clippy test

# The desktop app. Dioxus 0.7 auto-detects apps/mcpi/tailwind.css and runs
# Tailwind during the serve, which needs `bun install` to have resolved daisyui.
app:
    dx serve --package mcpi --platform desktop

# Rebuild the app's stylesheet by hand.
tw:
    bunx @tailwindcss/cli -i apps/mcpi/tailwind.css -o apps/mcpi/assets/tailwind.css
