# computer-use-mcp

An MCP server that gives AI agents a computer. Any MCP client, such as Claude Code or OpenCode, can see and control one shared Linux desktop that runs in Docker. You can watch and use the same desktop through VNC.

> **Status:** project setup only. The release pipeline, the container image, and a health endpoint work. The MCP tools and the desktop stack are not built yet.

## Design

- There is one computer. It is a Docker container with a Docker volume for its home folder.
- When an agent starts the MCP server, the server starts the container if it is stopped, or creates it if it does not exist. The server never stops the container.
- Every agent connected through the MCP server sees and controls the same desktop.
- Files in the volume survive container restarts and container deletion. Only you stop or delete the computer, with `docker stop` or `docker rm`.
- The container logs print the VNC link and credentials, so you can reopen a closed VNC session.

The project has three Rust crates:

| Crate | Runs on | Purpose |
| --- | --- | --- |
| `computer-use-mcp` | Your machine (Windows or macOS) | MCP server over stdio. Manages the container and opens the VNC view. |
| `computerd` | Inside the container | Captures the screen and drives mouse and keyboard input. |
| `computer-protocol` | Both | Request and response types shared by the two binaries. |

## Install

1. Download the archive for your platform from the [releases page](https://github.com/falleng0d/computer-use-mcp/releases). Builds exist for Windows x64 (`x86_64-pc-windows-msvc`) and macOS Apple Silicon (`aarch64-apple-darwin`).
2. Put `computer-use-mcp` on your `PATH`.
3. On macOS, the binary is not signed. If you downloaded it with a browser, remove the quarantine flag with `xattr -d com.apple.quarantine computer-use-mcp`.
4. The image lives in a private GitHub registry. Log in once with a token that has the `read:packages` scope, using `docker login ghcr.io -u <github-user>`.

Check the install with `computer-use-mcp info`. It prints the version and the image the binary uses.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `COMPUTER_USE_IMAGE` | Release builds use `ghcr.io/falleng0d/computer-use-mcp:<version>`. Dev builds use `computer-use-mcp:dev`. | Image used for the computer container. |

Release builds pull their image when it is missing. Dev builds never pull, so a dev host binary is never paired with an old image by accident. Build the dev image with `just image`.

## Development

Requirements: [Rust](https://rustup.rs) (the toolchain version comes from `rust-toolchain.toml`), [just](https://github.com/casey/just), Docker, and `gh`. On Windows, `just` runs recipes with Git Bash's `sh`.

```sh
just            # list recipes
just check      # fmt check, clippy with -D warnings, tests
just info       # run the host binary's info command
just image      # build the computer-use-mcp:dev image
just image-check  # build the image and call its health endpoint
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org). `feat:` and `fix:` decide the next version and the changelog entry.

## CI and releases

- Every branch push runs `just check` and builds the Windows and macOS binaries. The binaries are kept as workflow artifacts for 14 days.
- Merges to `master` update a release PR managed by release-plz. Merging that PR tags `vX.Y.Z` and creates the GitHub release.
- Every `v*` tag builds the host binaries and the `linux/amd64` + `linux/arm64` image. It uploads the binaries to the GitHub release and pushes the image to GHCR.
- Pre-releases come from any branch with `just tag-prerelease 0.2.0-beta.1`.

See [RELEASE.md](RELEASE.md) for the full flow and the one-time GitHub App setup.

## License

MIT. See [LICENSE](LICENSE).
