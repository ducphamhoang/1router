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
