# Releasing Waku

Waku auto-updates with [Sparkle](https://sparkle-project.org). Releases are
**GitHub Releases** of this repository: new users download a notarized
**`.dmg`** from the release page; existing users get updates via Sparkle, which
reads the signed appcast attached to the newest release at
`https://github.com/olioliverx/waku/releases/latest/download/appcast.xml`,
verifies each build's EdDSA signature, and installs it. One tag push produces
and publishes both.

Once set up, cutting a release is:

```sh
bun run release          # or: push a v* tag and let CI do it
```

- Updater code: [`src/updater.rs`](src/updater.rs) — loads the embedded
  Sparkle.framework at runtime and starts `SPUUpdater` with Waku's custom user
  driver. Available updates appear in the sidebar footer; download, signature
  verification, install, and relaunch remain owned by Sparkle. **Check for
  Updates…** lives in the app menu, and the **Automatic updates** toggle in
  Settings → General mirrors Sparkle's persisted setting.
- Feed URL + public key: [`resources/Info.plist`](resources/Info.plist)
  (`SUFeedURL`, `SUPublicEDKey`).
- Framework embedding + pinned Sparkle version:
  [`scripts/bundle.sh`](scripts/bundle.sh) (bump `sparkle_version` and
  `sparkle_sha256` together; the distribution is cached under
  `.waku-cache/sparkle/`).
- Release automation: [`scripts/release.ts`](scripts/release.ts),
  [`scripts/appcast.ts`](scripts/appcast.ts),
  [`scripts/changelog.ts`](scripts/changelog.ts).
- GitHub Actions: [`.github/workflows/release.yml`](.github/workflows/release.yml)
  builds Linux (x86_64, arm64) and macOS archives on a `v*` tag, attaches them
  (plus the signed `appcast.xml` and release notes) to a draft GitHub release.
  Publishing the draft is the whole publish step — GitHub serves the assets.

---

## One-time setup

The release runs on [Bun](https://bun.sh) and needs
[`create-dmg`](https://github.com/create-dmg/create-dmg)
(`brew install bun create-dmg`). No object storage is involved.

### 1. Sparkle signing keys

Updates are signed with an ed25519 key; the private half stays in the login
keychain (and is exported to the `SPARKLE_PRIVATE_KEY` repo secret for CI) and
the public half ships in Info.plist as `SUPublicEDKey`.

**This fork has its own key** — generated with the Sparkle tools and already
saved in the login keychain; the matching public key is already in Info.plist.
To export the private half again (e.g. to re-add the CI secret or restore a
backup), use the Sparkle tools (they land in
`.waku-cache/sparkle/<version>/bin` after any build, or download the release
from [sparkle-project/Sparkle](https://github.com/sparkle-project/Sparkle/releases)):

```sh
./bin/generate_keys -x sparkle_private_key.txt   # export the keychain key
./bin/generate_keys -p                            # prints the public key — must
                                                  # match SUPublicEDKey
```

On a fresh machine, restore the key from the password-manager backup:

```sh
./bin/generate_keys -f sparkle_private_key.txt   # import the backed-up key
```

> ⚠️ Lose the private key and existing installs can never update again. Keep
> the backup current.

> Note: upstream Waku signs with a different, older key. This fork deliberately
> uses its own key, so fork builds only trust fork-signed appcasts. Never
> replace `SUPublicEDKey` without re-signing every archived release.

### 2. Developer ID signing + notarization

Copy `.env.example` to `.env` and replace the signing placeholders. Analytics
endpoints are optional: leaving both `WAKU_ANALYTICS_ENDPOINT` and
`WAKU_ANALYTICS_WEBSITE_ID` unset disables analytics in the release build. The
script notarizes with the `NOTARY` keychain profile by default. On a fresh
machine:

```sh
cp .env.example .env
xcrun notarytool store-credentials NOTARY \
  --apple-id you@example.com --team-id YOUR_APPLE_TEAM_ID
```

Override the environment with `--signing-identity`, or change the notary
profile with `--notary-profile` / `WAKU_NOTARY_PROFILE`.

No bucket or CDN setup is needed: GitHub Releases hosts the binaries, the
release notes, and the appcast.

---

## Cutting a release

1. **Bump `version` in `Cargo.toml`** — the single source of truth.
   `CFBundleShortVersionString` is the version, and `CFBundleVersion` is
   derived from it (`major*1e6 + minor*1e3 + patch`, so `0.2.0` → `2000`),
   which keeps Sparkle's build-number comparison monotonic without a manual
   counter. Prerelease versions (`-beta.1`) are refused for publishing — the
   appcast serves one stable channel.
2. **Write the release notes** — add a `## [<version>]` section at the top of
   [`CHANGELOG.md`](CHANGELOG.md).
3. **Run it:**
   ```sh
   bun run release
   ```

The script builds and signs the app via `scripts/bundle.sh release`, verifies
the bundled JS REPL and computer-use helper, builds the styled DMG, notarizes
and staples DMG + app, zips the app for Sparkle, attaches the changelog
section as release notes, and regenerates the signed `appcast.xml`. When it
finishes:

- **Download link**: the `dist/Waku-<version>.dmg` it printed (or the GitHub
  release page)
- **In-app updates**: the app reads `SUFeedURL`, which points at this
  repository's `releases/latest/download/appcast.xml`.

Test by keeping an older build around, launching it, and choosing
**Check for Updates…**.

### GitHub release (CI)

Pushing a `v*` tag (matching the `version` in `Cargo.toml`) runs the Release
workflow. macOS CI runs `bun run release --local`, which signs, notarizes, and
writes the same artifacts as a local release:

- `Waku-<version>.dmg`
- `Waku-<version>.zip`
- `Waku-<version>.md` (Sparkle release notes)
- `appcast.xml` (Sparkle-signed; enclosure links point at this tag's assets)

Linux CI adds:

- `waku-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `waku-<version>-aarch64-unknown-linux-gnu.tar.gz`

The workflow opens (or updates) a **draft** GitHub release with those files and
the matching `CHANGELOG.md` section. **Publishing the draft release is the
entire publish step** — GitHub serves the binaries and the appcast
(`releases/latest/download/appcast.xml` resolves to the newest release's
asset), so there is no bucket to sync. Configure these repository secrets
first:

| Secret | Purpose |
| --- | --- |
| `WAKU_ANALYTICS_ENDPOINT` | optional; embedded in the macOS CI build |
| `WAKU_ANALYTICS_WEBSITE_ID` | optional; embedded in the macOS CI build |
| `WAKU_SIGNING_IDENTITY` | Developer ID identity selector |
| `APPLE_CERTIFICATE` | base64-encoded Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password for that `.p12` |
| `APPLE_ID` | Apple ID used by `notarytool` |
| `APPLE_APP_SPECIFIC_PASSWORD` | app-specific password for that Apple ID |
| `APPLE_TEAM_ID` | Developer Team ID |
| `SPARKLE_PRIVATE_KEY` | EdDSA private key for `generate_appcast` |

Without the Apple secrets the macOS job cannot sign/notarize; the Linux jobs
build with no secrets at all.

### Options

| Flag / Env | Default | Purpose |
| --- | --- | --- |
| `--local` | — | build, notarize, and write the DMG + zip without publishing |
| `--adhoc`, `--skip-notarize` | — | local test builds (imply `--local`) |
| `--skip-build` | — | reuse existing release binaries |
| `--build-number <n>` / `WAKU_BUILD_NUMBER` | derived | `CFBundleVersion` override |
| `WAKU_DOWNLOAD_URL_PREFIX` | `https://github.com/olioliverx/waku/releases/download/v<version>/` | base URL in the appcast |
| `SPARKLE_BIN` | the `.waku-cache` copy | Sparkle tools directory |

---

## Notes

- **Two artifacts per release:** the notarized `.dmg` (what people download)
  and a `.zip` (what Sparkle installs, plus `.delta` files against recent
  builds). Only the zip family appears in the appcast; point download buttons
  at the DMG.
- **Debug builds never update themselves.** `Updater::init` returns `None`
  under `debug_assertions`, so the dev watcher's app can't offer to replace
  itself with a production Waku. Set `WAKU_FORCE_UPDATER=1` to exercise the
  real Sparkle flow from a debug bundle anyway. A bare `cargo run` binary has
  no embedded framework and also degrades to no updater. For UI-only testing,
  start the watcher with `WAKU_PREVIEW_UPDATE=1`; the sidebar immediately
  shows an available update and clicking it changes to the spinner without
  installing anything. The preview flag fakes only that sidebar result;
  **Check for Updates…** still uses the embedded Sparkle framework and its
  real standard window.
- **Automatic and explicit checks have separate presentation.** Scheduled
  checks stay silent until the sidebar update button appears. Choosing
  **Check for Updates…** promotes an existing silent result into Sparkle's
  standard updater window, or shows its checking progress while an automatic
  check finishes. With no automatic session active, it starts Sparkle's
  standard user-initiated check directly.
- **First-run consent:** Sparkle shows its one-time "check automatically?"
  prompt on the second launch. The Settings → General toggle reads and writes
  the same persisted value.
- **Waku isn't sandboxed**, so Sparkle's XPC services are unnecessary;
  `bundle.sh` strips them (plus headers/modules) from the embedded framework
  and re-signs the rest with the app's identity — hardened-runtime library
  validation requires the identities to match.
- **Old archives stay attached to their GitHub releases** so far-behind users
  can still be served; only the recent history is staged locally under
  `dist/updates/` (git-ignored).
- **Platform artifacts:** GitHub Releases layout stays flat and
  platform-tagged by artifact name/extension — today's macOS names
  (`Waku-<v>.dmg`, `Waku-<v>.zip`, `appcast.xml`) must keep their URLs.
  Linux CI releases produce `waku-<v>-<target>.tar.gz` with
  `scripts/bundle-linux.sh` and land in the same GitHub release.
  Automatic Linux updates are not yet wired. Windows can later join with
  `Waku-<v>-Setup.exe` + `appcast-windows.xml` (WinSparkle reads the same
  appcast format). `src/updater.rs` is the per-platform seam, and everything
  mac-specific in the existing release pipeline lives behind the Darwin guard
  in `scripts/release.ts` plus `scripts/bundle.sh`.
- **Deltas are skipped**: each release's appcast is generated from just that
  release's archive, so updates are full downloads. GitHub hosts every older
  archive, so a future change could stage them for binary deltas again.
