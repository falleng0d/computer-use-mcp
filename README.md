# computer-use-mcp

An MCP server that gives AI agents a computer. Any MCP client, such as Claude Code or OpenCode, can see and control one shared Linux desktop that runs in Docker. You can watch and use the same desktop through VNC.

> **Status:** early. The MCP server starts the computer and hands out sessions (`start_computer`, `end_session`), shows each session its own screen (`computer_observe`), and lets it click, type, and scroll (`computer_act`), runs shell commands (`shell`, `set_cwd`), and lists, reads, and writes files (`list_files`, `read_file`, `write_file`).

## Design

- There is one computer. It is a Docker container with a Docker volume for its home folder.
- When an agent starts the MCP server, the server starts the container if it is stopped, or creates it if it does not exist. The server never stops the container.
- Every agent connected through the MCP server sees and controls the same desktop.
- Files in the volume survive container restarts and container deletion. Only you stop or delete the computer, with `docker stop` or `docker rm`.
- The computer is Debian 13 with Python 3, uv, Node.js 24 (LTS), Git, `gh`, the AWS CLI v2, Ruby, fish (bash stays the default shell), build tools, ripgrep, ImageMagick, clipboard tools, and fonts for Latin, CJK, and emoji.
- The user `computer` has passwordless `sudo`, so agents can `sudo apt-get install` more. Only home survives when the container is recreated, so system packages are lost then. `npm install -g` and `pip install` go to `~/.local` (first on `PATH`) and survive.
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

Add it to your agent host as an MCP server that runs `computer-use-mcp` with no arguments. Check the install with `computer-use-mcp info`. It prints the version and the image the binary uses.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `COMPUTER_USE_NAME` | `computer-use` | Name of the container. The home volume is `<name>-home`. Applies at creation. |
| `COMPUTER_USE_SCREEN_SIZE` | `1280x800` | Size of each session's screen, as `<width>x<height>` with sides from 320 to 7680. Read when `start_computer` runs. |
| `COMPUTER_USE_SHELL_TIMEOUT` | `120` | Seconds a `shell` command may run when the agent gives no timeout. Must not exceed the maximum. Read when `start_computer` runs. |
| `COMPUTER_USE_SHELL_TIMEOUT_MAX` | `600` | Longest timeout an agent may ask for, in seconds. If the default is unset and this is lower than 120, the default follows it. Read when `start_computer` runs. |
| `COMPUTER_USE_IDLE_TIMEOUT` | `1h` | Time without agent calls after which a session ends. Seconds (`90`) or a number with `s`, `m`, or `h` (`30m`, `2h`). Read when `start_computer` runs. |
| `COMPUTER_USE_IMAGE` | Release builds use `ghcr.io/falleng0d/computer-use-mcp:<version>`. Dev builds use `computer-use-mcp:dev`. | Image used for the computer container. |

Release builds pull their image when it is missing. Dev builds never pull, so a dev host binary is never paired with an old image by accident. Build the dev image with `just image`.

The server makes no Docker calls until an agent calls `start_computer`. That tool takes a title (1 to 80 characters) and returns a session id. The computer gets the host timezone and the `en_US.UTF-8` locale when it is created.

`computer_observe` takes the session id and returns a PNG of that session's own screen plus the frame id, capture time, size, cursor position, and active window title. The first call opens the screen, which is one `Xvnc` and Fluxbox inside the computer. The computer has 16 screens. A session that never calls it gets none, and ending the session closes its screen. When nothing changed since the session's previous screenshot, the image is left out and the text says so.

`computer_act` takes the session id and up to 24 ordered actions: `click`, `move`, `down`, `up`, `type`, `key`, `scroll`, `wait`, and `focus`. Positions are pixels on the session's screen. A double click counts as two actions. Waits and the settle time are capped at 5 s, and a scroll takes 1 to 20 steps. `type` handles any Unicode text. `key` takes names such as `enter`, `esc`, and `f5` with the modifiers `ctrl`, `alt`, `shift`, and `super` (also `cmd`, `option`, `meta`, `win`). `focus` raises an open window by its class or title. Launching apps comes later. The batch ends with a screenshot by default, taken `settle_ms` (default 300) after the last action, and the unchanged-frame rule applies to it. Set `observe` to false to skip it. The 4th identical batch of scroll, pointer, or key actions in a row that leaves the screen unchanged is refused.

`shell` takes the session id, a command, and an optional `timeout` in seconds. It runs `bash -lc` as the user `computer` inside the computer, in the session's working folder (home until `set_cwd` changes it), with no input. `DISPLAY` points at the session's screen when it has one. The result gives the exit code, the duration, and stdout and stderr separately. A failing command is a normal result. A command that runs past its timeout is killed with everything it started, and the result says so and keeps the output printed until then. Each stream keeps its first and last 15000 bytes with a `[... N bytes omitted ...]` marker between them. The call returns when the command exits, so start long-running jobs in the background with their output redirected, for example `setsid nohup server >log 2>&1 &`. Shell calls never wait for desktop actions, and one session may run several at once.

`set_cwd` takes the session id and a path, resolves a relative path from the current working folder (`~` is home), and fails when the folder does not exist. It returns the new absolute path.

`list_files`, `read_file`, and `write_file` take the session id and a path. Relative paths start at the session's working folder, `~` is home, and absolute paths work anywhere the user `computer` can reach. All sessions see the same files. `list_files` lists one folder (the working folder by default), folders first, with type, size, and modified time, and caps at 1000 entries. `read_file` returns UTF-8 text as text and PNG or JPEG files (up to 1 MB, found by their first bytes) as images. It refuses other binary files with a hint to use `shell`. Long text keeps its first and last 15000 bytes with a marker between them and a note, and the optional `offset` and `limit` (in lines, from 1) read a range. `write_file` replaces a file with UTF-8 content of up to 10 MB, creates missing folders, and writes atomically while keeping an existing file's permissions. It refuses a path that is a folder.

Sessions end on their own, so a crashed or forgotten agent does not hold a screen. Each MCP server process sends a heartbeat every 10 s for all its sessions, and its sessions end 30 s after the last one, which covers a killed process. A session also ends after its idle time with no agent call. A running `shell` command counts as activity, so a long command never makes its own session idle. When the MCP server exits on stdin close, Ctrl+C, or SIGTERM, it ends its sessions at once, waiting at most 3 s. Ending a session closes its screen and frees its number. Files in home are never touched. A call on a session that ended on its own says why and asks the agent to call `start_computer` again.

## Development

Requirements: [Rust](https://rustup.rs) (the toolchain version comes from `rust-toolchain.toml`), [just](https://github.com/casey/just), Docker, and `gh`. On Windows, `just` runs recipes with Git Bash's `sh`.

```sh
just            # list recipes
just check      # fmt check, clippy with -D warnings, tests
just info       # run the host binary's info command
just image      # build the computer-use-mcp:dev image
just image-check  # build the image, check every tool runs, and call the health endpoint
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
