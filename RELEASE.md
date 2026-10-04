# Release

Releases are automated with [release-plz](https://release-plz.dev) and GitHub Actions. You only merge PRs and, for pre-releases, push a tag.

## Stable releases

1. Run `just check` locally.
2. Commit with Conventional Commits (`feat: ...`, `fix: ...`) and push your branch. CI runs checks and builds the host binaries as artifacts.
3. Merge into `master`. release-plz opens or updates a release PR with the next version and the `CHANGELOG.md` entry.
4. Review and merge the release PR. release-plz tags `vX.Y.Z` and creates the GitHub release with the changelog notes.
5. The tag starts `release.yml`. It builds the Windows and macOS binaries and uploads them, with `SHA256SUMS.txt`, to the release. It pushes `ghcr.io/falleng0d/computer-use-mcp:X.Y.Z` and `:latest` for `linux/amd64` and `linux/arm64`.
6. After the image is pushed, `release.yml` publishes `computer-protocol`, `computer-transfer`, and `computer-use-mcp` to crates.io, in that order, with the `CARGO_REGISTRY_TOKEN` secret. Versions already on crates.io are skipped, so a rebuild is safe. `computerd` is not published. release-plz itself doesn't publish (`publish = false`).

Version bumps follow the commits since the last tag:

| Commit | From 1.0.0 | Before 1.0.0 |
| --- | --- | --- |
| `fix:` | patch | patch |
| `feat:` | minor | patch |
| Breaking (`feat!:` or a `BREAKING CHANGE:` footer) | major | minor |

All three crates share one version. A change in any crate, or in `crates/computerd/Dockerfile`, bumps the shared version.

## Pre-releases

Pre-releases can come from any branch, and they don't need a release PR.

```sh
just tag-prerelease 0.2.0-beta.1
```

This pushes the tag `v0.2.0-beta.1`. `release.yml` builds the binaries with that version embedded, creates a GitHub pre-release, and pushes the image as `:0.2.0-beta.1`. It does not move `:latest`.

- Use a dot before the number (`beta.1`, not `beta-1`). Semver then sorts `beta.10` after `beta.9`.
- Pre-release tags don't change `CHANGELOG.md` or `Cargo.toml`. release-plz ignores them when it computes the next stable version.

## Rebuilding a release

If a release job fails after the tag exists, rerun the failed jobs in GitHub, or rebuild from scratch.

```sh
just gh-rebuild-release v0.2.0
```

Uploads use `--clobber`, so a rebuild replaces existing assets.

## One-time setup: crates.io token

Create a crates.io API token with the `publish-update` scope (and `publish-new` until all three crates exist), then store it:

```sh
gh secret set CARGO_REGISTRY_TOKEN --repo falleng0d/computer-use-mcp
```

Pre-release tags are not published to crates.io. A crates.io build counts as a release build because Cargo adds `.cargo_vcs_info.json` to packaged crates (`crates/computer-protocol/build.rs`), so `cargo install` uses the image of its own version.

## One-time setup: GitHub App for release-plz

Tags and PRs created with the default `GITHUB_TOKEN` don't trigger other workflows. release-plz therefore uses a GitHub App token, so its tags start `release.yml` and its release PRs run CI.

1. Open <https://github.com/settings/apps/new>.
2. Set a unique name, for example `falleng0d-release-plz`, and use the repository URL as the homepage URL.
3. Under **Webhook**, clear **Active**.
4. Under **Repository permissions**, set **Contents** and **Pull requests** to **Read and write**.
5. Under **Where can this GitHub App be installed?**, choose **Only on this account**, then create the app.
6. Copy the **Client ID**. Under **Private keys**, generate a key and download the `.pem` file.
7. Open **Install App**, install it on your account, and select only `computer-use-mcp`.
8. Store the values in the repository:

   ```sh
   gh variable set RELEASE_PLZ_APP_CLIENT_ID --repo falleng0d/computer-use-mcp --body "<client id>"
   gh secret set RELEASE_PLZ_APP_PRIVATE_KEY --repo falleng0d/computer-use-mcp < path/to/key.pem
   ```

9. Start the release-plz workflow with `just gh-release-pr`, or push to `master`.

## Local helpers

| Recipe | What it does |
| --- | --- |
| `just version` | Runs `release-plz update` locally to preview the version bump and changelog. Needs `release-plz` on `PATH`. Discard the changes afterwards. |
| `just gh-release-pr` | Runs the release-plz workflow on `master`. |
| `just gh-runs release.yml` | Lists recent release runs. |
| `just gh-watch <run-id>` | Follows a run until it finishes. |
