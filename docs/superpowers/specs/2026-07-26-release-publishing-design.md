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

## Jobs

Three jobs run on tag push. `binaries` and `docker` are independent of
each other (a failure in one doesn't block the other); `release` depends
on `binaries`.

### `binaries`

Matrix of 4 native (runner, target) pairs — no cross-compilation:

| Runner | Target |
|---|---|
| `ubuntu-latest` | `x86_64-unknown-linux-musl` |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` |
| `macos-13` | `x86_64-apple-darwin` |
| `macos-14` | `aarch64-apple-darwin` |

Each leg:
1. Checkout, install the Rust target via `rustup target add`.
2. On the two Linux legs, `apt-get install musl-tools` (static musl libc
   needed for the `-musl` targets; not required on macOS).
3. `cargo build --release --target <target>` — network-enabled, unlike
   the Codex sandbox's `--offline` builds elsewhere in this project.
4. Strip the binary, package as `1router-<tag>-<target>.tar.gz`.
5. Upload as a workflow artifact.

`fail-fast: true` (the default): if any leg fails, the whole job fails,
and a release is all-platforms-or-nothing rather than partial.

### `release`

Needs `binaries`. Downloads all 4 artifacts, runs
`sha256sum * > SHA256SUMS`, then creates/updates the GitHub Release for
the tag via `softprops/action-gh-release`, attaching the 4 tarballs plus
`SHA256SUMS`.

### `docker`

Independent of `binaries`/`release`. Sets up QEMU + `docker buildx`, logs
into `ghcr.io` using the default `GITHUB_TOKEN` (no new secret needed),
and builds the existing `Dockerfile` with
`--platform linux/amd64,linux/arm64`, pushing one multi-arch manifest as
both `ghcr.io/ducphamhoang/1router:<tag>` and `:latest`. The Dockerfile
already compiles the binary inside the container rather than copying a
prebuilt artifact in, so QEMU-emulated `arm64` compilation is acceptable
(well-trodden for Alpine/musl builds) rather than needing native arm64
Docker build hosts.

## Data flow

```
tag push (v*.*.*)
        |
        +--> binaries (4-way matrix, parallel) --> release (attach 4 tarballs + SHA256SUMS)
        |
        +--> docker (buildx multi-arch) --> ghcr.io/.../1router:<tag> and :latest
```

## Error handling

- Any `binaries` matrix leg failing fails the whole `binaries` job (and
  skips `release`) — no partial-platform releases.
- `docker` job failing is independent; recovery is a manual re-run of
  that job from the Actions UI (no automatic retry or re-tagging
  plumbing needed for v1).

## Testing / verification

No unit tests apply to CI YAML. Verification is manual, on first real
tag push (e.g. `v0.1.0`):
- Confirm the GitHub Release has 4 binary tarballs + `SHA256SUMS`.
- Confirm `docker pull ghcr.io/ducphamhoang/1router:v0.1.0` works, and
  `docker manifest inspect` shows both `linux/amd64` and `linux/arm64`.

## Out of scope (v1)

- Windows binaries.
- Automatic Cargo.toml version bumping / changelog generation.
- Automatic retry of a failed `docker` job on tag push.
