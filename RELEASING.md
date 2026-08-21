# Releasing mcpi

One-time setup, then the per-release steps. Everything here is macOS-only; the
other platforms are not shipped yet.

---

## One-time

### Apple Developer Program

Required for codesigning and notarization; an unsigned `.app` gets a Gatekeeper
wall.  ~€99/year, and enrolment takes days — start it well before you need it.

You need a **Developer ID Application** certificate (not Mac App Store), then:

```sh
# The identity string, for Dioxus.toml
security find-identity -v -p codesigning
```

Put it in `apps/mcpi/Dioxus.toml` under `[bundle.macos]`:

```toml
signing_identity = "Developer ID Application: Your Name (TEAMID)"
```

Store an app-specific password for notarytool:

```sh
xcrun notarytool store-credentials mcpi-notary \
  --apple-id you@example.com --team-id TEAMID --password <app-specific-password>
```

---

## Per release

```sh
just check                      # must be green
dx bundle --package mcpi --platform desktop --release
```

Output — note the doubled `macos/`, and that `release/macos/` is a *different*,
stale artifact left by `dx build`:

```
target/dx/mcpi/bundle/macos/macos/mcpi.app
target/dx/mcpi/bundle/macos/macos/mcpi_<version>_aarch64.dmg
```

The `.dmg` is produced automatically, so there is nothing to assemble by hand.
Worth keeping the size honest in any marketing, since "a Rust binary, not
Electron" is a claim people check.

**`dx build` ignores most of `[bundle]`** — it produces a runnable `.app` for
development only. Any packaging change has to be verified with a bundle.

Verify the metadata really landed, because it fails silently:

```sh
plutil -p target/dx/mcpi/bundle/macos/macos/mcpi.app/Contents/Info.plist \
  | grep -E 'CFBundleIdentifier|LSMinimumSystemVersion|LSApplicationCategory'
```

Expect `app.mcpi.desktop`, `11.0`, `public.app-category.developer-tools`. If you
see `com.example.mcpi` or `10.15` you are looking at the `release/macos/`
artifact instead of the bundled one — a mistake worth naming, because both paths
exist and only one is real.

Then sign, notarize, and staple:

```sh
APP=target/dx/mcpi/bundle/macos/macos/mcpi.app

codesign --deep --force --options runtime --timestamp \
  --sign "Developer ID Application: Your Name (TEAMID)" "$APP"

ditto -c -k --keepParent "$APP" mcpi.zip
xcrun notarytool submit mcpi.zip --keychain-profile mcpi-notary --wait
xcrun stapler staple "$APP"

# What a user's machine will actually check:
spctl -a -vvv "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
```

Ship the stapled `.app` in a zip or dmg. Stapling matters: without it the app
needs network access on first launch to verify, and someone opening it on a
plane sees a scary dialog.

---

## Things that will bite

- **The bundle identifier is load-bearing.** macOS keys keychain ACLs to it, so
  changing `app.mcpi.desktop` after release orphans every user's stored OAuth
  tokens — they would all have to sign in again with no explanation.
- **Entitlements are not embedded until you sign for real.** An ad-hoc signature
  ignores them, so `codesign -d --entitlements -` on a local bundle shows
  nothing. That is expected; verify them after signing with a Developer ID, not
  before.
- **Hardened runtime is on from the first bundle.** Turning it on late is how
  you find out which entitlements were missing, at the worst moment. Current
  entitlements are `network-client` (dialling MCP servers) and `network-server`
  (the loopback listener the OAuth redirect lands on).
- **Keychain prompts are expected on unsigned builds** and stop once the app is
  signed with a stable identity. If they persist after signing, the identity is
  changing between builds.
- **The icons are generated from `assets/logo.svg`.** To regenerate after a logo
  change: nest the tile at 824/1024 on a transparent canvas (Apple's icon grid),
  render each iconset size with `rsvg-convert`, then `iconutil -c icns`.

## Not built yet

- Auto-update. v1 can be "check for updates" opening the download page.
- Linux and Windows bundles.
