# Release Publishing (GHCR image + binary releases) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On push of a `v*.*.*` tag, publish prebuilt `1router` binaries for 4 platforms to a GitHub Release and a multi-arch `linux/amd64` + `linux/arm64` image to `ghcr.io/ducphamhoang/1router`.

**Architecture:** One new workflow, `.github/workflows/release.yml`, with three sequential jobs — `binaries` (4-leg native matrix, no cross-compilation) → `release` (SHA256SUMS + `softprops/action-gh-release`) → `docker` (QEMU + buildx multi-arch push). The existing root `Dockerfile` is first made `TARGETARCH`-aware so the arm64 leg of the buildx build actually compiles an aarch64 binary instead of silently shipping the x86_64 one.

**Tech Stack:** GitHub Actions (`actions/checkout@v4`, `actions/upload-artifact@v4`, `actions/download-artifact@v4`, `softprops/action-gh-release@v2`, `docker/setup-qemu-action@v3`, `docker/setup-buildx-action@v3`, `docker/login-action@v3`, `docker/build-push-action@v6`), `rustup`, `musl-tools`, Docker buildx, GHCR.

## Global Constraints

- Workflow file path is exactly `.github/workflows/release.yml`; trigger is push of tags matching `v*.*.*`.
- Tagging is manual. The tag name is both the GitHub Release name and the Docker image tag. `Cargo.toml`'s `version` is NOT force-synced to the tag; a mismatch is cosmetic and out of scope.
- Re-pushing the same tag is allowed and overwrites the previous result (release assets and image manifest) rather than failing.
- Job order and dependencies are exactly: `binaries` → `release` (`needs: binaries`) → `docker` (`needs: release`). `docker` must NOT run in parallel with `release`, so `:latest` only moves after a complete binary release.
- Per-job permissions: `binaries` = `contents: read`; `release` = `contents: write`; `docker` = `contents: read` + `packages: write`.
- `binaries` matrix is exactly 4 native (runner, target) pairs, no cross-compilation: `ubuntu-24.04`/`x86_64-unknown-linux-musl`, `ubuntu-24.04-arm`/`aarch64-unknown-linux-musl`, `macos-15-intel`/`x86_64-apple-darwin`, `macos-14`/`aarch64-apple-darwin`.
- Runners are pinned explicitly — never `ubuntu-latest`/`macos-latest`.
- `fail-fast: true` on the matrix (the default): a release is all-platforms-or-nothing, never partial.
- Tarball naming is exactly `1router-<tag>-<target>.tar.gz`.
- Workflow artifacts use `retention-days: 7` — the GitHub Release, not the artifact, is the durable channel.
- Release builds are network-enabled: use plain `cargo build --release --target <target>`, NOT the `--offline` convention used elsewhere in this repo for the Codex sandbox.
- `musl-tools` is installed only on the two Linux legs; not required on macOS.
- Docker image tags pushed: `ghcr.io/ducphamhoang/1router:<tag>` AND `ghcr.io/ducphamhoang/1router:latest`, from a single multi-arch manifest built with `--platform linux/amd64,linux/arm64`.
- GHCR login uses the default `GITHUB_TOKEN` — no new repository secret.
- Dockerfile arch mapping is exactly `amd64` → `x86_64-unknown-linux-musl`, `arm64` → `aarch64-unknown-linux-musl`.
- Dockerfile must carry `org.opencontainers.image.source` pointing at `https://github.com/ducphamhoang/1router` to document the GHCR package↔repo link.
- Out of scope (v1): Windows binaries, automatic `Cargo.toml` version bumping, changelog generation, automatic retry of a failed `docker` job, failing (rather than overwriting) on re-tagging.
- Project context: Cargo package/lib name is `router`, binary target name is `1router` — the compiled artifact is always `target/<triple>/release/1router`.

---

### Task 1: TARGETARCH-aware Dockerfile

Today `Dockerfile` hard-codes `--target x86_64-unknown-linux-musl` in both the `cargo build` and the runtime-stage `COPY`. Under `docker buildx build --platform linux/amd64,linux/arm64` that would produce an arm64 image containing an x86_64 binary. This task makes the build stage resolve the Rust target from buildx's `TARGETARCH` and stage the result at a fixed, arch-independent path so the `COPY` no longer names an architecture.

**Files:**
- Modify: `Dockerfile:1-19` (whole file rewritten)
- Modify: `.cargo/config.toml:1-2` (add an `aarch64-unknown-linux-musl` section alongside the existing x86_64 one — the file is `COPY`'d into the builder stage, so the arm64 leg needs the same `+crt-static` rustflags)
- Test: `docker buildx build --platform linux/amd64,linux/arm64 .` (multi-platform), with a decisive single-arch fallback described in Step 1

**Interfaces:**
- Consumes: nothing (first task).
- Produces: a root `Dockerfile` that honours `--platform linux/amd64,linux/arm64`; the label `org.opencontainers.image.source=https://github.com/ducphamhoang/1router`. Task 4's `docker` job builds this file with `context: .`.

- [ ] **Step 1: Write the verification script**

Create the verification script (a scratch file, not committed):

```bash
mkdir -p /tmp/1router-release-verify
cat > /tmp/1router-release-verify/verify-dockerfile-arch.sh <<'EOF'
#!/usr/bin/env bash
# Verifies the Dockerfile produces a native binary for each requested platform.
# Preferred path: a true multi-platform buildx build (requires the
# docker-container driver + binfmt/QEMU registered for arm64).
# Fallback path: two single-platform --load builds, extracting /1router from
# each image and asserting its ELF machine type with `file`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

echo "== attempting multi-platform build =="
if docker buildx build --platform linux/amd64,linux/arm64 -t 1router:multiarch-verify . ; then
  echo "MULTIARCH_BUILD=ok"
else
  echo "MULTIARCH_BUILD=unavailable (see fallback below)"
fi

echo "== per-arch binary machine-type check =="
check_arch() {
  platform="$1"; expect="$2"; tag="1router:verify-${3}"
  docker buildx build --platform "$platform" -t "$tag" --load .
  cid="$(docker create --platform "$platform" "$tag")"
  docker cp "$cid:/1router" "/tmp/1router-release-verify/1router-${3}"
  docker rm "$cid" >/dev/null
  out="$(file "/tmp/1router-release-verify/1router-${3}")"
  echo "$out"
  echo "$out" | grep -q "$expect" || { echo "FAIL: expected '$expect' for $platform"; exit 1; }
  echo "OK: $platform -> $expect"
}
check_arch linux/amd64 "x86-64" amd64
check_arch linux/arm64 "aarch64" arm64
echo "ALL OK"
EOF
chmod +x /tmp/1router-release-verify/verify-dockerfile-arch.sh
```

Note on the local constraint: a `--platform linux/amd64,linux/arm64` build cannot be `--load`ed into the classic docker image store (multi-platform manifests need `--push` or the containerd image store), and the arm64 leg needs QEMU/binfmt registered. If `docker buildx create --driver docker-container` and `docker run --privileged --rm tonistiigi/binfmt --install arm64` are not possible on this machine, the two `check_arch` calls are the authoritative test for the *native* arch and the cross-arch leg must be treated as verified-in-CI (Task 6) instead. Do not claim multi-arch success from Dockerfile reading alone; record which of the two paths actually ran.

- [ ] **Step 2: Run the verification against the current Dockerfile to see it fail**

Prepare the builder and binfmt (best effort — continue if either fails, the fallback path still exercises the native arch):

```bash
docker buildx create --name 1router-multiarch --driver docker-container --use || docker buildx use 1router-multiarch
docker run --privileged --rm tonistiigi/binfmt --install arm64
```

Run: `/tmp/1router-release-verify/verify-dockerfile-arch.sh`

Expected: FAIL. With the current hard-coded Dockerfile, the `linux/arm64` leg either (a) errors in the build stage because `rust:1.90-alpine` on arm64 has no `x86_64-unknown-linux-musl` target installed — `error[E0463]`/`can't find crate for 'std'` for `x86_64-unknown-linux-musl` — or (b) if it gets that far, `check_arch linux/arm64` prints `... x86-64 ...` and the script exits with `FAIL: expected 'aarch64' for linux/arm64`.

- [ ] **Step 3: Add aarch64 musl rustflags to `.cargo/config.toml`**

Full new content of `.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.aarch64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]
```

- [ ] **Step 4: Rewrite the Dockerfile to be TARGETARCH-aware**

Full new content of `Dockerfile`:

```dockerfile
# ---- build stage ----
FROM rust:1.90-alpine AS builder
# rustls (not openssl) handles TLS and sqlx's sqlite feature bundles/statically
# links libsqlite3, so no OpenSSL dependency is actually needed at build or
# runtime - musl-dev + sqlite-static is sufficient.
RUN apk add --no-cache musl-dev sqlite-static pkgconfig
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY migrations ./migrations
COPY src ./src
# TARGETARCH is supplied by buildx (docker/amd64 -> "amd64", docker/arm64 ->
# "arm64"). Map it to the matching Rust musl triple, then stage the binary at a
# fixed, arch-independent path so the runtime-stage COPY needs no arch in it
# (COPY cannot run shell, so it cannot do the mapping itself).
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    cargo build --release --target "$RUST_TARGET"; \
    cp "target/${RUST_TARGET}/release/1router" /app/1router

# ---- runtime stage ----
FROM gcr.io/distroless/static-debian12
LABEL org.opencontainers.image.source="https://github.com/ducphamhoang/1router"
COPY --from=builder /app/1router /1router
ENV ROUTER_LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/1router"]
```

(`rustup target add` is a no-op when the triple is already the image's host target — which it is for both legs, since `rust:1.90-alpine` is itself musl-hosted — but it keeps the mapping honest if the base image ever changes.)

- [ ] **Step 5: Run the verification to confirm it passes**

Run: `/tmp/1router-release-verify/verify-dockerfile-arch.sh`

Expected: `MULTIARCH_BUILD=ok` (or `unavailable` with the reason recorded), then

```
OK: linux/amd64 -> x86-64
OK: linux/arm64 -> aarch64
ALL OK
```

- [ ] **Step 6: Smoke-test the built image serves /health**

```bash
docker run -d --name 1router-smoke -p 18080:8080 \
  -e ROUTER_SHARED_SECRET=smoketest \
  -e ROUTER_SQLITE_PATH=/tmp/1router.db \
  1router:verify-amd64
sleep 3
curl -fsS http://127.0.0.1:18080/health; echo
docker rm -f 1router-smoke
```

Expected: a JSON body from `/health` and HTTP 200 (`curl -f` exits 0).

- [ ] **Step 7: Commit**

```bash
git add Dockerfile .cargo/config.toml
git commit -m "build: make Dockerfile TARGETARCH-aware for multi-arch buildx"
```

---

### Task 2: `binaries` matrix job

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `/tmp/1router-release-verify/check-release-yml.py` (scratch validator, not committed)
- Test: `python3 /tmp/1router-release-verify/check-release-yml.py`

**Interfaces:**
- Consumes: nothing from Task 1 (independent file), but assumes the repo's binary target is named `1router`.
- Produces: workflow `name: release`; job id `binaries`; 4 uploaded artifacts named `1router-<tag>-<target>` each containing `1router-<tag>-<target>.tar.gz`. Task 3's `release` job references `needs: binaries` and downloads these artifacts.

- [ ] **Step 1: Write the validator**

```bash
mkdir -p /tmp/1router-release-verify
cat > /tmp/1router-release-verify/check-release-yml.py <<'EOF'
#!/usr/bin/env python3
"""Structural validator for .github/workflows/release.yml.

Grows with the plan: BINARIES-only assertions in Task 2, plus RELEASE in
Task 3, plus DOCKER in Task 4. Requires only stdlib + PyYAML (PyYAML is
already importable in this environment; verify with
`python3 -c "import yaml"`).
"""
import sys, yaml

EXPECT_JOBS = set(sys.argv[1:]) or {"binaries"}

with open(".github/workflows/release.yml") as fh:
    wf = yaml.safe_load(fh)

# PyYAML parses the bare key `on` as the boolean True (YAML 1.1 truthiness),
# so accept either spelling.
trigger = wf.get("on", wf.get(True))
assert trigger is not None, "no `on:` trigger found"
assert trigger["push"]["tags"] == ["v*.*.*"], trigger
assert wf["name"] == "release", wf["name"]

jobs = wf["jobs"]
assert set(jobs) == EXPECT_JOBS, f"jobs {sorted(jobs)} != {sorted(EXPECT_JOBS)}"

if "binaries" in EXPECT_JOBS:
    b = jobs["binaries"]
    assert b["permissions"] == {"contents": "read"}, b["permissions"]
    assert b["strategy"]["fail-fast"] is True, b["strategy"]
    pairs = {(m["runner"], m["target"]) for m in b["strategy"]["matrix"]["include"]}
    assert pairs == {
        ("ubuntu-24.04", "x86_64-unknown-linux-musl"),
        ("ubuntu-24.04-arm", "aarch64-unknown-linux-musl"),
        ("macos-15-intel", "x86_64-apple-darwin"),
        ("macos-14", "aarch64-apple-darwin"),
    }, pairs
    uses = [s.get("uses", "") for s in b["steps"]]
    assert any(u.startswith("actions/checkout@") for u in uses), uses
    upload = [s for s in b["steps"] if s.get("uses", "").startswith("actions/upload-artifact@")]
    assert len(upload) == 1, uses
    assert upload[0]["with"]["retention-days"] == 7, upload[0]["with"]
    runs = "\n".join(s.get("run", "") for s in b["steps"])
    assert "musl-tools" in runs, "musl-tools install missing"
    assert "rustup target add" in runs, "rustup target add missing"
    assert "cargo build --release --target" in runs, "release build missing"
    assert "--offline" not in runs, "release builds must be network-enabled"
    assert "strip" in runs, "strip step missing"
    assert "tar -czf" in runs, "tarball packaging missing"

if "release" in EXPECT_JOBS:
    r = jobs["release"]
    assert r["needs"] == "binaries", r["needs"]
    assert r["permissions"] == {"contents": "write"}, r["permissions"]
    uses = [s.get("uses", "") for s in r["steps"]]
    assert any(u.startswith("actions/download-artifact@") for u in uses), uses
    assert any(u.startswith("softprops/action-gh-release@") for u in uses), uses
    runs = "\n".join(s.get("run", "") for s in r["steps"])
    assert "sha256sum" in runs and "SHA256SUMS" in runs, runs
    ghr = [s for s in r["steps"] if s.get("uses", "").startswith("softprops/action-gh-release@")][0]
    files = ghr["with"]["files"]
    assert "dist/*.tar.gz" in files and "dist/SHA256SUMS" in files, files

if "docker" in EXPECT_JOBS:
    d = jobs["docker"]
    assert d["needs"] == "release", d["needs"]
    assert d["permissions"] == {"contents": "read", "packages": "write"}, d["permissions"]
    uses = [s.get("uses", "") for s in d["steps"]]
    for prefix in ("actions/checkout@", "docker/setup-qemu-action@",
                   "docker/setup-buildx-action@", "docker/login-action@",
                   "docker/build-push-action@"):
        assert any(u.startswith(prefix) for u in uses), f"missing {prefix}: {uses}"
    login = [s for s in d["steps"] if s.get("uses", "").startswith("docker/login-action@")][0]
    assert login["with"]["registry"] == "ghcr.io", login["with"]
    assert "secrets.GITHUB_TOKEN" in login["with"]["password"], login["with"]
    bp = [s for s in d["steps"] if s.get("uses", "").startswith("docker/build-push-action@")][0]
    assert bp["with"]["platforms"] == "linux/amd64,linux/arm64", bp["with"]
    assert bp["with"]["push"] is True, bp["with"]
    tags = bp["with"]["tags"]
    assert "ghcr.io/ducphamhoang/1router:latest" in tags, tags
    assert "ghcr.io/ducphamhoang/1router:${{ github.ref_name }}" in tags, tags

print("release.yml OK for jobs:", ", ".join(sorted(EXPECT_JOBS)))
EOF
chmod +x /tmp/1router-release-verify/check-release-yml.py
```

- [ ] **Step 2: Run the validator to see it fail**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries`

Expected: `FileNotFoundError: [Errno 2] No such file or directory: '.github/workflows/release.yml'`

- [ ] **Step 3: Create `.github/workflows/release.yml` with the `binaries` job**

```bash
mkdir -p .github/workflows
```

Full content of `.github/workflows/release.yml`:

```yaml
name: release

on:
  push:
    tags:
      - 'v*.*.*'

jobs:
  binaries:
    name: binaries (${{ matrix.target }})
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
    strategy:
      # Default, but stated explicitly: a release is all-platforms-or-nothing.
      fail-fast: true
      matrix:
        include:
          - runner: ubuntu-24.04
            target: x86_64-unknown-linux-musl
          - runner: ubuntu-24.04-arm
            target: aarch64-unknown-linux-musl
          - runner: macos-15-intel
            target: x86_64-apple-darwin
          - runner: macos-14
            target: aarch64-apple-darwin
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install musl build tools (Linux legs only)
        if: startsWith(matrix.runner, 'ubuntu')
        run: |
          sudo apt-get update
          sudo apt-get install -y musl-tools

      - name: Add Rust target
        run: rustup target add ${{ matrix.target }}

      - name: Build release binary
        run: cargo build --release --target ${{ matrix.target }}

      - name: Strip binary
        run: |
          BIN="target/${{ matrix.target }}/release/1router"
          case "${{ matrix.target }}" in
            *-apple-darwin) strip -x "$BIN" ;;
            *)              strip "$BIN" ;;
          esac

      - name: Package tarball
        run: |
          tar -czf "1router-${GITHUB_REF_NAME}-${{ matrix.target }}.tar.gz" \
            -C "target/${{ matrix.target }}/release" 1router

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: 1router-${{ github.ref_name }}-${{ matrix.target }}
          path: 1router-${{ github.ref_name }}-${{ matrix.target }}.tar.gz
          retention-days: 7
```

Two deliberate details: `strip -x` on the Apple targets (a full `strip` can invalidate the ad-hoc code signature on arm64 macOS binaries and produce a killed process at runtime), and `tar -C target/<triple>/release 1router` so the tarball contains a bare `1router` at its root rather than a nested `target/...` path.

- [ ] **Step 4: Run the validator to confirm it passes**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries`

Expected: `release.yml OK for jobs: binaries`

- [ ] **Step 5: Confirm the tarball naming matches the plan's contract**

Run:

```bash
python3 - <<'PY'
import yaml
wf = yaml.safe_load(open('.github/workflows/release.yml'))
steps = wf['jobs']['binaries']['steps']
pkg = [s for s in steps if s['name'] == 'Package tarball'][0]['run']
up  = [s for s in steps if s.get('name') == 'Upload artifact'][0]['with']
assert '1router-${GITHUB_REF_NAME}-${{ matrix.target }}.tar.gz' in pkg, pkg
assert up['path'] == '1router-${{ github.ref_name }}-${{ matrix.target }}.tar.gz', up
print('naming OK: 1router-<tag>-<target>.tar.gz')
PY
```

Expected: `naming OK: 1router-<tag>-<target>.tar.gz`

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add release workflow with 4-platform binaries matrix job"
```

---

### Task 3: `release` job

**Files:**
- Modify: `.github/workflows/release.yml` (append a `release:` job after `binaries:`)
- Test: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release`

**Interfaces:**
- Consumes: job id `binaries` (via `needs: binaries`) and its 4 artifacts named `1router-<tag>-<target>`, each containing `1router-<tag>-<target>.tar.gz`.
- Produces: job id `release`, which Task 4's `docker` job references as `needs: release`. Also produces the GitHub Release for `${{ github.ref_name }}` with 4 tarballs + `SHA256SUMS` attached.

- [ ] **Step 1: Extend the verification to require the `release` job**

The validator written in Task 2 already contains the `release` assertions, gated on the job being requested on the command line. No edit needed — the "failing test" is simply invoking it with the `release` job included.

- [ ] **Step 2: Run the validator to see it fail**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release`

Expected: `AssertionError: jobs ['binaries'] != ['binaries', 'release']`

- [ ] **Step 3: Append the `release` job to `.github/workflows/release.yml`**

Append, at the same indentation level as `binaries:` (2 spaces), after the `binaries` job's last step:

```yaml
  release:
    name: release
    needs: binaries
    runs-on: ubuntu-24.04
    permissions:
      contents: write
    steps:
      - name: Download all binary artifacts
        uses: actions/download-artifact@v4
        with:
          path: dist
          merge-multiple: true

      - name: Generate SHA256SUMS
        working-directory: dist
        run: |
          sha256sum *.tar.gz > SHA256SUMS
          cat SHA256SUMS

      - name: Create or update GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          tag_name: ${{ github.ref_name }}
          name: ${{ github.ref_name }}
          files: |
            dist/*.tar.gz
            dist/SHA256SUMS
```

Notes on the two non-obvious choices:
- `merge-multiple: true` flattens all 4 artifacts into a single `dist/` directory. Without it, `actions/download-artifact@v4` nests each under `dist/<artifact-name>/`, and the `dist/*.tar.gz` glob would match nothing.
- The spec writes `sha256sum * > SHA256SUMS`; this uses `sha256sum *.tar.gz` so the in-progress `SHA256SUMS` file can never be globbed into its own input.
- `softprops/action-gh-release@v2` replaces existing assets by default, which is exactly the re-tagging overwrite policy the spec requires; no extra flag needed.

- [ ] **Step 4: Run the validator to confirm it passes**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release`

Expected: `release.yml OK for jobs: binaries, release`

- [ ] **Step 5: Dry-run the SHA256SUMS shell logic locally**

Run:

```bash
rm -rf /tmp/1router-release-verify/dist && mkdir -p /tmp/1router-release-verify/dist
cd /tmp/1router-release-verify/dist
for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin; do
  printf 'fake\n' > "1router-v0.0.1-rc1-$t.tar.gz"
done
sha256sum *.tar.gz > SHA256SUMS
test "$(wc -l < SHA256SUMS)" -eq 4 && ! grep -q SHA256SUMS SHA256SUMS && echo "SHA256SUMS logic OK (4 entries, no self-reference)"
```

Expected: `SHA256SUMS logic OK (4 entries, no self-reference)`

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add release job publishing tarballs and SHA256SUMS"
```

---

### Task 4: `docker` job (multi-arch GHCR push)

**Files:**
- Modify: `.github/workflows/release.yml` (append a `docker:` job after `release:`)
- Test: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release docker`

**Interfaces:**
- Consumes: job id `release` (via `needs: release`); the `TARGETARCH`-aware `Dockerfile` from Task 1; the default `GITHUB_TOKEN`.
- Produces: job id `docker`; the images `ghcr.io/ducphamhoang/1router:${{ github.ref_name }}` and `ghcr.io/ducphamhoang/1router:latest` as one multi-arch manifest. Task 5 configures that package's visibility; Task 6 pulls and inspects both tags.

- [ ] **Step 1: Extend the verification to require the `docker` job**

The validator written in Task 2 already contains the `docker` assertions, gated on the job being requested on the command line. No edit needed.

- [ ] **Step 2: Run the validator to see it fail**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release docker`

Expected: `AssertionError: jobs ['binaries', 'release'] != ['binaries', 'docker', 'release']`

- [ ] **Step 3: Append the `docker` job to `.github/workflows/release.yml`**

Append, at the same indentation level as `release:` (2 spaces), after the `release` job's last step:

```yaml
  # Depends on `release` (not parallel with it) so ghcr.io/.../1router:latest
  # only ever moves once the full binary release for this tag has succeeded.
  docker:
    name: docker
    needs: release
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      packages: write
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Set up QEMU
        uses: docker/setup-qemu-action@v3

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build and push multi-arch image
        uses: docker/build-push-action@v6
        with:
          context: .
          file: ./Dockerfile
          platforms: linux/amd64,linux/arm64
          push: true
          provenance: false
          tags: |
            ghcr.io/ducphamhoang/1router:${{ github.ref_name }}
            ghcr.io/ducphamhoang/1router:latest
```

Notes:
- QEMU is required because the arm64 leg compiles Rust from source inside the container; there is no native arm64 builder here. This is slower than native but is the well-trodden path for Alpine/musl builds.
- `provenance: false` keeps the pushed manifest list to exactly the two platform entries, so `docker manifest inspect` in Task 6 shows `linux/amd64` and `linux/arm64` and not an extra `unknown/unknown` attestation entry.
- No new secret is needed: `packages: write` plus the default `GITHUB_TOKEN` authorizes the GHCR push. **This only holds if the `1router` GHCR package is linked to this repository — see Task 5, which must be completed before or immediately after the first push attempt.**

- [ ] **Step 4: Run the validator to confirm it passes**

Run: `python3 /tmp/1router-release-verify/check-release-yml.py binaries release docker`

Expected: `release.yml OK for jobs: binaries, release, docker`

- [ ] **Step 5: Verify the whole job graph matches the spec's data flow**

Run:

```bash
python3 - <<'PY'
import yaml
wf = yaml.safe_load(open('.github/workflows/release.yml'))
jobs = wf['jobs']
assert 'needs' not in jobs['binaries'], jobs['binaries'].get('needs')
assert jobs['release']['needs'] == 'binaries'
assert jobs['docker']['needs'] == 'release'
print('graph OK: binaries -> release -> docker')
PY
```

Expected: `graph OK: binaries -> release -> docker`

- [ ] **Step 6: Confirm the Dockerfile the job builds is the TARGETARCH-aware one**

Run:

```bash
grep -q 'ARG TARGETARCH' Dockerfile \
  && grep -q 'aarch64-unknown-linux-musl' Dockerfile \
  && grep -q 'org.opencontainers.image.source' Dockerfile \
  && ! grep -q 'COPY --from=builder /app/target/x86_64' Dockerfile \
  && echo "Dockerfile is multi-arch ready"
```

Expected: `Dockerfile is multi-arch ready`

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add docker job pushing multi-arch image to GHCR"
```

---

### Task 5: GHCR package linkage and visibility (one-time manual prerequisite)

GHCR defaults new packages to **private**, and `GITHUB_TOKEN` can only publish a package associated with the workflow's own repository. If a `1router` package already exists under `ghcr.io/ducphamhoang` disconnected from this repo, the push in Task 4 fails with a permissions error that no workflow change can fix — it requires manual re-linking in the package settings. This task records the checklist in the repo so it is not rediscovered from a red CI run, and performs the manual steps.

**Files:**
- Create: `docs/superpowers/plans/2026-07-26-release-publishing-ghcr-checklist.md`
- Test: manual — `gh api /users/ducphamhoang/packages/container/1router` and an anonymous `docker pull` (both commands given below)

**Interfaces:**
- Consumes: the image `ghcr.io/ducphamhoang/1router:<tag>` pushed by job `docker` (Task 4), and the `org.opencontainers.image.source` label from Task 1.
- Produces: a public, repo-linked GHCR package `1router`. Task 6's `docker pull` (run without `docker login`) depends on this.

- [ ] **Step 1: Write the checklist file**

Full content of `docs/superpowers/plans/2026-07-26-release-publishing-ghcr-checklist.md`:

```markdown
# GHCR package setup — one-time manual steps (first release only)

The `release.yml` `docker` job pushes to `ghcr.io/ducphamhoang/1router` using
the default `GITHUB_TOKEN` with `packages: write`. That works only for a
package that is **associated with this repository**, and GHCR creates new
packages as **private**. Do this once, on the first release:

1. Push a tag and let the `docker` job run at least once so the package exists:
   <https://github.com/ducphamhoang/1router/actions/workflows/release.yml>

2. Confirm the package exists and see its visibility:

       gh api /users/ducphamhoang/packages/container/1router \
         --jq '{name, visibility, repository: .repository.full_name}'

   Expected once configured:

       {"name":"1router","visibility":"public","repository":"ducphamhoang/1router"}

3. If `visibility` is `private`, make it public:
   <https://github.com/users/ducphamhoang/packages/container/1router/settings>
   → "Danger Zone" → "Change visibility" → Public → type `1router` to confirm.

4. If `repository` is `null` (package not linked to the repo), link it on the
   same settings page → "Manage Actions access" / "Connect repository" → select
   `ducphamhoang/1router` → give it the `Write` role. The
   `org.opencontainers.image.source` label in the root `Dockerfile` documents
   the intended link but does not establish it by itself.

5. Verify an unauthenticated pull works (log out of GHCR first, so the check
   really is anonymous):

       docker logout ghcr.io
       docker pull ghcr.io/ducphamhoang/1router:latest

   A `denied` / `unauthorized` error here means step 3 was not applied.

## Failure signature if skipped

The `docker` job's "Build and push multi-arch image" step fails with:

    denied: installation not allowed to Write organization package

or

    unauthorized: unauthenticated

Fix by applying steps 3 and 4, then re-run the failed `docker` job from the
Actions UI (Re-run failed jobs) — no re-tagging needed.
```

- [ ] **Step 2: Check the current package state (expected: not yet existing)**

Run: `gh api /users/ducphamhoang/packages/container/1router --jq '{name, visibility, repository: .repository.full_name}'`

Expected, before any release has run: `gh: Package not found (HTTP 404)`. That is the correct "not yet done" state; the remaining steps are executed as part of Task 6, after the first `docker` job has actually pushed.

- [ ] **Step 3: Commit the checklist**

```bash
git add docs/superpowers/plans/2026-07-26-release-publishing-ghcr-checklist.md
git commit -m "docs: GHCR package linkage/visibility checklist for first release"
```

- [ ] **Step 4: Apply the manual settings (do this during Task 6, after the first push)**

Follow steps 3–5 of the checklist above in the GitHub UI, then re-run:

```bash
gh api /users/ducphamhoang/packages/container/1router \
  --jq '{name, visibility, repository: .repository.full_name}'
```

Expected: `{"name":"1router","visibility":"public","repository":"ducphamhoang/1router"}`

---

### Task 6: End-to-end verification with a real test tag

CI YAML cannot be unit-tested; the spec's verification is manual on a real tag push. This task pushes a throwaway `v0.0.1-rc1` tag (it matches the `v*.*.*` glob — `*` matches `1-rc1`), watches all three jobs, and checks the release assets and the GHCR manifest, then cleans up.

**Files:**
- Modify: none (verification only)
- Test: the `gh`/`docker`/`curl` commands below, run against the real Actions run

**Interfaces:**
- Consumes: jobs `binaries`, `release`, `docker` from `.github/workflows/release.yml`; the `TARGETARCH`-aware `Dockerfile`; the GHCR checklist from Task 5.
- Produces: nothing in the repo — a verified release pipeline plus a recorded pass/fail for each spec verification bullet.

- [ ] **Step 1: Push the branch and confirm `gh` is authenticated**

```bash
gh auth status
git rev-parse --abbrev-ref HEAD          # expect: impl/v1
git push origin HEAD
```

Expected: `gh auth status` reports a logged-in account with `repo` and `read:packages` scopes; the push succeeds.

Note before proceeding: this test tag will move `ghcr.io/ducphamhoang/1router:latest` onto an rc image. That is acceptable and expected (registry tags are mutable); pushing the real `v0.1.0` afterwards moves `:latest` back onto the real release.

- [ ] **Step 2: Push the test tag**

```bash
git tag v0.0.1-rc1
git push origin v0.0.1-rc1
```

Expected: `* [new tag] v0.0.1-rc1 -> v0.0.1-rc1`

- [ ] **Step 3: Watch the run to completion**

```bash
sleep 10
RUN_ID=$(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')
echo "run: $RUN_ID"
gh run watch "$RUN_ID" --exit-status
```

Expected: all jobs green and exit code 0. The 4 `binaries` legs finish in minutes; the `docker` job's arm64 leg compiles under QEMU and can take 20–40 minutes — that is normal, not a hang.

If the `docker` job fails with `denied:` / `unauthorized:`, stop and apply Task 5 Step 4 (GHCR visibility + repo link), then:

```bash
gh run rerun "$RUN_ID" --failed
gh run watch "$RUN_ID" --exit-status
```

- [ ] **Step 4: Verify the release assets (spec bullet 1: 4 tarballs + SHA256SUMS)**

```bash
gh release view v0.0.1-rc1 --json assets -q '.assets[].name' | sort
```

Expected exactly these 5 lines:

```
1router-v0.0.1-rc1-aarch64-apple-darwin.tar.gz
1router-v0.0.1-rc1-aarch64-unknown-linux-musl.tar.gz
1router-v0.0.1-rc1-x86_64-apple-darwin.tar.gz
1router-v0.0.1-rc1-x86_64-unknown-linux-musl.tar.gz
SHA256SUMS
```

- [ ] **Step 5: Verify the checksums actually match the published tarballs**

```bash
rm -rf /tmp/1router-rc && mkdir -p /tmp/1router-rc
gh release download v0.0.1-rc1 --dir /tmp/1router-rc
cd /tmp/1router-rc && sha256sum -c SHA256SUMS
```

Expected: 4 lines each ending in `: OK`.

- [ ] **Step 6: Verify the Linux x86_64 tarball contains a working static binary**

```bash
cd /tmp/1router-rc
tar -xzf 1router-v0.0.1-rc1-x86_64-unknown-linux-musl.tar.gz
file ./1router
./1router --help | head -5
```

Expected: `file` reports `ELF 64-bit LSB ... x86-64 ... statically linked`, and `--help` prints usage without a dynamic-loader error.

- [ ] **Step 7: Verify the GHCR image pulls anonymously (spec bullet 3)**

```bash
docker logout ghcr.io
docker pull ghcr.io/ducphamhoang/1router:v0.0.1-rc1
docker pull ghcr.io/ducphamhoang/1router:latest
gh api /users/ducphamhoang/packages/container/1router \
  --jq '{name, visibility, repository: .repository.full_name}'
```

Expected: both pulls succeed, and `{"name":"1router","visibility":"public","repository":"ducphamhoang/1router"}`.

- [ ] **Step 8: Verify the manifest advertises both platforms (spec bullet 2)**

```bash
docker manifest inspect ghcr.io/ducphamhoang/1router:v0.0.1-rc1 \
  | python3 -c 'import json,sys; m=json.load(sys.stdin); print(sorted(f"{p[\"platform\"][\"os\"]}/{p[\"platform\"][\"architecture\"]}" for p in m["manifests"]))'
```

Expected: `['linux/amd64', 'linux/arm64']`

- [ ] **Step 9: Verify each platform really contains its native binary**

```bash
docker run --privileged --rm tonistiigi/binfmt --install arm64
for pair in "linux/amd64 x86-64" "linux/arm64 aarch64"; do
  set -- $pair
  cid=$(docker create --platform "$1" ghcr.io/ducphamhoang/1router:v0.0.1-rc1)
  docker cp "$cid:/1router" "/tmp/1router-rc/bin-$(echo "$1" | tr / -)"
  docker rm "$cid" >/dev/null
  file "/tmp/1router-rc/bin-$(echo "$1" | tr / -)" | grep -q "$2" \
    && echo "OK: $1 contains $2" || { echo "FAIL: $1 does not contain $2"; }
done
```

Expected:

```
OK: linux/amd64 contains x86-64
OK: linux/arm64 contains aarch64
```

- [ ] **Step 10: Smoke-test the arm64 image under emulation serving /health**

```bash
docker run -d --name 1router-rc-arm64 --platform linux/arm64 -p 18081:8080 \
  -e ROUTER_SHARED_SECRET=smoketest \
  -e ROUTER_SQLITE_PATH=/tmp/1router.db \
  ghcr.io/ducphamhoang/1router:v0.0.1-rc1
sleep 8
curl -fsS http://127.0.0.1:18081/health; echo
docker logs 1router-rc-arm64 | tail -20
docker rm -f 1router-rc-arm64
```

Expected: `curl -f` exits 0 with a JSON `/health` body; logs show a normal startup with no panic. (`ROUTER_SHARED_SECRET` is set so the headless boot path never tries to run the interactive wizard, and `ROUTER_SQLITE_PATH=/tmp/1router.db` keeps the DB off the read-only-by-convention image root.)

- [ ] **Step 11: Clean up the test tag, release, and GHCR versions**

```bash
gh release delete v0.0.1-rc1 --yes --cleanup-tag
git tag -d v0.0.1-rc1
docker image rm ghcr.io/ducphamhoang/1router:v0.0.1-rc1 ghcr.io/ducphamhoang/1router:latest || true
# Optional: remove the rc version from GHCR (keeps the package listing clean).
# Find its id, then delete it:
gh api /users/ducphamhoang/packages/container/1router/versions \
  --jq '.[] | select(.metadata.container.tags[]? == "v0.0.1-rc1") | .id'
# gh api -X DELETE /user/packages/container/1router/versions/<ID>
```

Expected: the release and tag are gone (`gh release view v0.0.1-rc1` → `release not found`). Leaving the rc version in GHCR is harmless; `:latest` will be re-pointed by the first real `v0.1.0` tag.

- [ ] **Step 12: Record the verification result in the progress ledger**

```bash
cat >> .superpowers/sdd/progress.md <<'EOF'

## Release publishing — e2e verification (v0.0.1-rc1)
- binaries/release/docker jobs: green
- release assets: 4 tarballs + SHA256SUMS, `sha256sum -c` all OK
- GHCR: public, linked to ducphamhoang/1router; manifest = linux/amd64 + linux/arm64
- per-arch binary check: amd64 -> x86-64, arm64 -> aarch64
- arm64 /health smoke under QEMU: 200 OK
EOF
```

(`.superpowers/sdd/progress.md` is git-ignored scratch — nothing to commit here. There is no code change in this task, so this task has no commit step.)

---

## Self-Review

**1. Spec coverage** — every section of `docs/superpowers/specs/2026-07-26-release-publishing-design.md` maps to a task:

| Spec section | Task |
|---|---|
| Goal | Tasks 1–6 collectively |
| Trigger & versioning (`v*.*.*`, manual tagging, no version sync) | Task 2 Step 3 (`on.push.tags`), Global Constraints |
| Re-tagging policy (overwrite, don't fail) | Task 3 Step 3 note (`action-gh-release` replaces assets), Global Constraints |
| Jobs — ordering rationale (`docker` needs `release`) | Task 4 Step 3 comment + Step 5 graph check |
| Jobs — `binaries` (4-pair matrix, pinned runners, permissions, musl-tools, build, strip, tar.gz, retention-days 7, fail-fast) | Task 2 |
| Jobs — `release` (needs, contents:write, download, sha256sum, action-gh-release with 4 tarballs + SHA256SUMS) | Task 3 |
| Jobs — `docker` (needs release, contents:read + packages:write, QEMU + buildx, ghcr login via GITHUB_TOKEN, multi-arch `:<tag>` + `:latest`) | Task 4 |
| Dockerfile must become platform-aware (`ARG TARGETARCH`, amd64/arm64 mapping, both build and COPY) | Task 1 |
| `org.opencontainers.image.source` label | Task 1 Step 4, checked in Task 4 Step 6 |
| Operational prerequisite (GHCR package link + public visibility, first release only) | Task 5 (+ referenced from Task 4 Step 3 and Task 6 Step 3/7) |
| Data flow diagram | Task 4 Step 5 asserts the exact graph |
| Error handling (fail-fast, manual re-run of `docker`, re-tag overwrite) | Task 2 Step 3 (`fail-fast: true`), Task 6 Step 3 (`gh run rerun --failed`), Task 3 Step 3 note |
| Testing / verification (4 tarballs + SHA256SUMS; `docker pull`; `docker manifest inspect` both platforms; correct native binary / `/health` under emulation; GHCR public+linked) | Task 6 Steps 4–10 |
| Out of scope (v1) | Global Constraints (listed, no task) |

**2. Placeholder scan** — no `TBD`, no "add appropriate …", no "similar to Task N". Every YAML block is complete and copy-pasteable (Task 2 gives the full file; Tasks 3 and 4 give complete appended job blocks with explicit 2-space indentation stated). Every shell block is runnable as written. The only intentionally commented-out command is the destructive `gh api -X DELETE .../versions/<ID>` in Task 6 Step 11, whose `<ID>` comes from the command printed immediately above it.

**3. Type/name consistency across tasks** —
- Job ids: `binaries` (defined Task 2) is referenced as `needs: binaries` in Task 3 and asserted in the validator; `release` (defined Task 3) is referenced as `needs: release` in Task 4 and asserted in Task 4 Step 5; `docker` (defined Task 4) is referenced in Task 5 and Task 6. All three spellings are lowercase and identical everywhere.
- Rust triples appear identically in the Dockerfile mapping (Task 1), `.cargo/config.toml` (Task 1), the matrix (Task 2), the validator's expected pairs (Task 2), and the expected asset names (Task 6 Step 4): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`.
- Artifact name `1router-${{ github.ref_name }}-${{ matrix.target }}` (Task 2) and its file `1router-<tag>-<target>.tar.gz` are what Task 3's `merge-multiple: true` + `dist/*.tar.gz` glob consumes and what Task 6 Step 4 expects.
- Image reference is `ghcr.io/ducphamhoang/1router` in Task 4, Task 5's checklist, and Task 6 — never a bare `1router` or a different owner.
- Binary path is `target/<triple>/release/1router` (Cargo `[[bin]] name = "1router"`) in Task 1's `cp`, Task 2's strip/tar, consistent with the repo's existing Dockerfile.
- Validator script path `/tmp/1router-release-verify/check-release-yml.py` is created once in Task 2 and re-invoked with additional job arguments in Tasks 3 and 4.
