# Release publishing (GHCR image + binary releases) — Design

## Goal

Let users get a working `1router` without cloning and building the repo
themselves: a multi-arch Docker image on GHCR, and prebuilt binaries
attached to GitHub Releases.

## Trigger & versioning

A new GitHub Actions workflow, `.github/workflows/release.yml`, fires on
push of a tag matching `v*.*.*` (e.g. `v0.1.0`). Tagging is manual — the
user decides when to cut a release and creates the tag themselves. The
tag name becomes both the GitHub Release name and the Docker image tag.
`Cargo.toml`'s `version` field is not force-synced to the tag; a mismatch
between the two is cosmetic only and out of scope for v1.

**Re-tagging policy**: pushing the same tag again (e.g. to fix a broken
release) is allowed and overwrites the previous result rather than
failing — `softprops/action-gh-release` replaces existing release assets
by default, and a re-pushed Docker tag simply overwrites the prior
manifest in GHCR (registry tags are always mutable; this is normal
Docker semantics, not something a workflow can prevent). This is
acceptable for v1 given tagging is a manual, low-frequency action.

## Jobs

Three jobs run on tag push, in this order: `binaries` → `release` →
`docker`. `docker` deliberately depends on `release` (not run in
parallel) so that `ghcr.io/.../1router:latest` only ever moves once the
full binary release for that tag has succeeded — otherwise GHCR could
advertise a version with no corresponding, complete GitHub Release.

### `binaries`

Matrix of 4 native (runner, target) pairs — no cross-compilation:

| Runner | Target |
|---|---|
| `ubuntu-24.04` | `x86_64-unknown-linux-musl` |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` |
| `macos-15-intel` | `x86_64-apple-darwin` |
| `macos-14` | `aarch64-apple-darwin` |

(`ubuntu-24.04` is pinned explicitly rather than `ubuntu-latest`, so the
release build environment doesn't silently drift when GitHub rolls the
`-latest` alias forward. `macos-15-intel` replaces the earlier
`macos-13` — GitHub's Intel macOS runner label — since `macos-13` is not
a currently supported hosted-runner image.)

Permissions: `contents: read` (checkout only; no packages/releases
access needed at this stage).

Each leg:
1. Checkout, install the Rust target via `rustup target add`.
2. On the two Linux legs, `apt-get install musl-tools` (static musl libc
   needed for the `-musl` targets; not required on macOS).
3. `cargo build --release --target <target>` — network-enabled, unlike
   the Codex sandbox's `--offline` builds elsewhere in this project.
4. Strip the binary, package as `1router-<tag>-<target>.tar.gz`.
5. Upload as a workflow artifact, `retention-days: 7` (short-lived — the
   GitHub Release, not the workflow artifact, is the durable
   distribution channel).

`fail-fast: true` (the default): if any leg fails, the whole job fails,
and a release is all-platforms-or-nothing rather than partial.

### `release`

Needs `binaries`. Permissions: `contents: write` (to create the Release
and upload assets). Downloads all 4 artifacts, runs
`sha256sum * > SHA256SUMS`, then creates/updates the GitHub Release for
the tag via `softprops/action-gh-release`, attaching the 4 tarballs plus
`SHA256SUMS`.

### `docker`

Needs `release` (see re-ordering note above). Permissions: `contents:
read` + `packages: write` (required to push to GHCR under the default
`GITHUB_TOKEN`). Sets up QEMU + `docker buildx`, logs into `ghcr.io`
using the default `GITHUB_TOKEN` (no new secret needed, given the
`packages: write` permission), and builds the existing `Dockerfile` with
`--platform linux/amd64,linux/arm64`, pushing one multi-arch manifest as
both `ghcr.io/ducphamhoang/1router:<tag>` and `:latest`.

The Dockerfile must become platform-aware for this to actually produce
correct arm64 output: today it hard-codes
`--target x86_64-unknown-linux-musl` for both the `cargo build` and the
binary `COPY`, so a naive `--platform linux/amd64,linux/arm64` build
would silently ship an x86_64 binary inside the arm64 image. The fix is
a buildx-provided `ARG TARGETARCH`, mapped to the right Rust target
(`amd64` → `x86_64-unknown-linux-musl`, `arm64` →
`aarch64-unknown-linux-musl`) before the `cargo build`/`COPY` steps.
Compiling the arm64 leg under QEMU emulation (rather than a native arm64
builder) is acceptable here since the Dockerfile already compiles from
source inside the container — this is a well-trodden path for
Alpine/musl builds, just slower than native.

**Operational prerequisite (one-time, manual)**: on the very first
release, confirm the `1router` package that appears under
`ghcr.io/ducphamhoang` is linked to this repository and its visibility
is set to public (GHCR defaults new packages to private). `GITHUB_TOKEN`
can publish a package associated with the workflow's repo, but if a
package of the same name already exists disconnected from the repo, the
push will fail with a permissions error requiring manual re-linking in
the package settings. Adding an `org.opencontainers.image.source` label
to the Dockerfile documents the link explicitly.

## Data flow

```
tag push (v*.*.*)
        |
        v
   binaries (4-way matrix, parallel)
        |
        v
     release (attach 4 tarballs + SHA256SUMS)
        |
        v
     docker (buildx multi-arch) --> ghcr.io/.../1router:<tag> and :latest
```

## Error handling

- Any `binaries` matrix leg failing fails the whole `binaries` job (and
  skips `release`, which in turn skips `docker`) — no partial-platform
  releases, and `:latest` never moves for an incomplete release.
- `docker` job failing after `release` succeeded leaves the GitHub
  Release published without an updated image; recovery is a manual
  re-run of that job from the Actions UI.
- Re-pushing the same tag re-runs all three jobs and overwrites the
  prior release assets and image manifest (see Re-tagging policy above).

## Testing / verification

No unit tests apply to CI YAML. Verification is manual, on first real
tag push (e.g. `v0.1.0`):
- Confirm the GitHub Release has 4 binary tarballs + `SHA256SUMS`.
- Confirm `docker pull ghcr.io/ducphamhoang/1router:v0.1.0` works, and
  `docker manifest inspect` shows both `linux/amd64` and `linux/arm64`
  with each running the correct native binary (e.g. `docker run --rm
  --platform linux/arm64 ... uname -m` style sanity check, or simply
  confirming the container starts and serves `/health` under emulation).
- Confirm the GHCR package is public and linked to the repo (first
  release only).

## Out of scope (v1)

- Windows binaries.
- Automatic Cargo.toml version bumping / changelog generation.
- Automatic retry of a failed `docker` job on tag push.
- Failing (rather than overwriting) on re-tagging an existing version.
