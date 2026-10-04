# computer-use-mcp

An MCP server that gives AI agents a computer. Any MCP client, such as Claude Code or OpenCode, can see and control one shared Linux desktop that runs in Docker. You can watch and use the same desktop in a browser or with any VNC client.

> **Status:** early. The MCP server starts the computer and hands out sessions (`start_computer`, `end_session`), shows each session its own screen (`computer_observe`), and lets it click, type, and scroll (`computer_act`), runs shell commands (`shell`, `set_cwd`), lists, reads, and writes files (`list_files`, `read_file`, `write_file`), and opens pages, files, and applications on the screen (`open_path`, `launch_app`).

## Design

- There is one computer. It is a Docker container with a Docker volume for its home folder.
- When an agent starts the MCP server, the server starts the container if it is stopped, or creates it if it does not exist. The server never stops the container. The only time it removes one is a stopped container on an older image than the binary's, which it creates again on the newer image with the same home volume and the same ports. It never touches a running container and never moves the computer to an older image. To upgrade, run `docker stop` on the computer and call `start_computer`. `computer-use-mcp info` shows whether an upgrade is pending.
- Every agent connected through the MCP server sees and controls the same desktop.
- Files in the volume survive container restarts and container deletion. Only you stop or delete the computer, with `docker stop` or `docker rm`.
- The computer is Debian 13 with Python 3, uv, Node.js 24 (LTS), Git, `gh`, the AWS CLI v2, Ruby, fish (bash stays the default shell), build tools, ripgrep, ImageMagick, LibreOffice (Writer, Calc, Impress), clipboard tools, and fonts for Latin, CJK, and emoji.
- The user `computer` has passwordless `sudo`, so agents can `sudo apt-get install` more. Only home survives when the container is recreated, so system packages are lost then. `npm install -g` and `pip install` go to `~/.local` (first on `PATH`) and survive.
- The container logs print the viewer link and the VNC password, so you can reopen a closed view.

The project has three Rust crates:

| Crate | Runs on | Purpose |
| --- | --- | --- |
| `computer-use-mcp` | Your machine (Windows or macOS) | MCP server over stdio. Manages the container and opens the VNC view. |
| `computerd` | Inside the container | Runs the sessions and their screens, captures the screen, drives mouse and keyboard input, serves the viewer, runs Chromium with cookie sync, and handles shell commands and files. |
| `computer-protocol` | Both | Request and response types shared by the two binaries. |

## Install

1. Download the archive for your platform from the [releases page](https://github.com/falleng0d/computer-use-mcp/releases). Builds exist for Windows x64 (`x86_64-pc-windows-msvc`) and macOS Apple Silicon (`aarch64-apple-darwin`).
2. Put `computer-use-mcp` on your `PATH`.
3. On macOS, the binary is not signed. If you downloaded it with a browser, remove the quarantine flag with `xattr -d com.apple.quarantine computer-use-mcp`.
4. The image lives in a private GitHub registry. Log in once with a token that has the `read:packages` scope, using `docker login ghcr.io -u <github-user>`.

Add it to your agent host as an MCP server that runs `computer-use-mcp` with no arguments. Check the install with `computer-use-mcp info`. It prints the version, the image the binary uses, and the state, image version, and pending upgrade of the computer.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `COMPUTER_USE_NAME` | `computer-use` | Name of the container. The home volume is `<name>-home`. Applies at creation. |
| `COMPUTER_USE_SCREEN_SIZE` | `1280x800` | Size of each session's screen, as `<width>x<height>` with sides from 320 to 7680. Read when `start_computer` runs. |
| `COMPUTER_USE_SHELL_TIMEOUT` | `120` | Time a `shell` command may run when the agent gives no timeout, as seconds (`90`) or a number with `s`, `m`, or `h` (`2m`). Must not exceed the maximum. Read when `start_computer` runs. |
| `COMPUTER_USE_SHELL_TIMEOUT_MAX` | `600` | Longest timeout an agent may ask for, in the same forms. If the default is unset and this is lower than 120, the default follows it. Read when `start_computer` runs. |
| `COMPUTER_USE_IDLE_TIMEOUT` | `1h` | Time without agent calls after which a session ends. Same forms (`90`, `90s`, `30m`, `2h`). Read when `start_computer` runs. |
| `COMPUTER_USE_PORT_BASE` | `20900` | First of the 17 host ports the computer publishes on `127.0.0.1` (a port from 1024 to 65519). Applies at creation. |
| `COMPUTER_USE_OPEN` | `browser` | What opens when one of this server's sessions gets its screen: `browser`, `vnc`, or `none`. Read when `start_computer` runs. See "Opening the viewer". |
| `COMPUTER_USE_VNC_VIEWER` | unset | Windows only. Path of the VNC viewer that `vnc` mode runs. See "Opening the viewer". |
| `COMPUTER_USE_IMAGE` | `ghcr.io/falleng0d/computer-use-mcp:<version>` in release builds, `computer-use-mcp:dev` in dev builds | Image used for the computer container. |

Release builds pull their image when it is missing. Dev builds never pull, so a dev host binary is never paired with an old image by accident. Build the dev image with `just image`.

The server makes no Docker calls until an agent calls `start_computer`. That tool takes a title (1 to 80 characters) and returns a session id. The computer gets the host timezone and the `en_US.UTF-8` locale when it is created.

`computer_observe` takes the session id and returns a PNG of that session's own screen plus the frame id, capture time, size, cursor position, and active window title. The first call opens the screen, which is one `Xvnc` and Fluxbox inside the computer. The computer has 16 screens. A session that never calls it gets none, and ending the session closes its screen. When nothing changed since the session's previous screenshot, the image is left out and the text says so.

`computer_act` takes the session id and up to 24 ordered actions: `click`, `move`, `down`, `up`, `type`, `key`, `scroll`, `wait`, and `focus`. Positions are pixels on the session's screen. A double click counts as two actions. Waits and the settle time are capped at 5 s, and a scroll takes 1 to 20 steps. `type` handles any Unicode text. `key` takes names such as `enter`, `esc`, and `f5` with the modifiers `ctrl`, `alt`, `shift`, and `super` (also `cmd`, `option`, `meta`, `win`). `focus` raises a window by its class or title, or starts the app when no window matches. The batch ends with a screenshot by default, taken `settle_ms` (default 300) after the last action, and the unchanged-frame rule applies to it. Set `observe` to false to skip it. The 4th identical batch of scroll, pointer, or key actions in a row that leaves the screen unchanged is refused.

`shell` takes the session id, a command, and an optional `timeout` in seconds. It runs `bash -lc` as the user `computer` inside the computer, in the session's working folder (home until `set_cwd` changes it), with no input. `DISPLAY` points at the session's screen when it has one. The result gives the exit code, the duration, and stdout and stderr separately. A failing command is a normal result. A command that runs past its timeout is killed with everything it started, and the result says so and keeps the output printed until then. Each stream keeps its first and last 15000 bytes with a `[... N bytes omitted ...]` marker between them. The call returns when the command exits, so start long-running jobs in the background with their output redirected, for example `setsid nohup server >log 2>&1 &`. Shell calls never wait for desktop actions, and one session may run several at once.

`set_cwd` takes the session id and a path, resolves a relative path from the current working folder (`~` is home), and fails when the folder does not exist. It returns the new absolute path.

`list_files`, `read_file`, and `write_file` take the session id and a path. Relative paths start at the session's working folder, `~` is home, and absolute paths work anywhere the user `computer` can reach. All sessions see the same files. `list_files` lists one folder (the working folder by default), folders first, with type, size, and modified time, and caps at 1000 entries. `read_file` returns UTF-8 text as text and PNG or JPEG files (up to 1 MB, found by their first bytes) as images. It refuses other binary files with a hint to use `shell`. Long text keeps its first and last 15000 bytes with a marker between them and a note, and the optional `offset` and `limit` (in lines, from 1) read a range. `write_file` replaces a file with UTF-8 content of up to 10 MB, creates missing folders, and writes atomically while keeping an existing file's permissions. It refuses a path that is a folder.

Sessions end on their own, so a crashed or forgotten agent does not hold a screen. Each MCP server process sends a heartbeat every 10 s for all its sessions, and its sessions end 30 s after the last one, which covers a killed process. A session also ends after its idle time with no agent call. A running `shell` command counts as activity, so a long command never makes its own session idle. When the MCP server exits on stdin close, Ctrl+C, or SIGTERM, it ends its sessions at once, waiting at most 3 s. Ending a session closes its screen and frees its number. Files in home are never touched. A call on a session that ended on its own says why and asks the agent to call `start_computer` again.

## Docker endpoint

The server talks to the same Docker as your `docker` command. It picks the endpoint in this order.

1. `DOCKER_HOST`.
2. The context named by `DOCKER_CONTEXT`.
3. `currentContext` in `~/.docker/config.json` (the folder moves with `DOCKER_CONFIG`).
4. The platform default, a named pipe on Windows and `/var/run/docker.sock` elsewhere.

The context `default` means step 4. Colima, OrbStack, and Rancher Desktop work after `docker context use <name>` (for example `colima`, `orbstack`, or `rancher-desktop`). Run `computer-use-mcp info` to see the endpoint in use and why it was chosen.

Supported endpoints are `unix://`, `npipe://`, `tcp://`, and `http://`. A context that needs TLS or uses `ssh://` is refused with a message that names it. Use a context with a socket, or set `DOCKER_HOST`.

## Watching and using the screens

`start_computer` returns a viewer link such as `http://127.0.0.1:20900/#key=ab3d5fgh`, and `computer-use-mcp info` prints it while the computer runs. The container logs print it at every start (`docker logs <name>`). Open it in a browser. The page lists every live session in a sidebar with its title, screen number, start time, and number of viewers, and updates as sessions start and end. Click a screen to connect to it. A bar above the screen shows its number, title, and size. Every connection starts locked (`View only`), so watching never sends a stray click or keypress. Press `Unlock` to take control and `Lock` to hand back. Nothing coordinates you with the agent while you are unlocked. The lock lives in the page only and does not affect native VNC clients. The browser keeps the key in local storage and removes it from the address bar. The address bar keeps `#screen=<n>` so a link can open the page on one screen.

Native VNC clients connect to `127.0.0.1` on the base port plus the screen number (`20901` to `20916` by default) and use the key as the password. macOS Screen Sharing works with `open vnc://:<key>@127.0.0.1:20901`. The key is 8 characters because classic VNC password authentication ignores everything after the 8th. `computerd` makes it on the first start with a fresh home and keeps it in home, so links stay valid across restarts.

Ports published on `127.0.0.1` only:

| Host port | Serves |
| --- | --- |
| base | Viewer page and its WebSocket bridge for noVNC (v1.7.0) |
| base + 1 to base + 16 | Raw VNC for screens 1 to 16 |
| random port | The `computerd` API that the MCP server calls. Container port 7070, protected by a bearer token. |

The API port is picked by Docker when the container is created. Port 7071 (the launcher for the Fluxbox menu), 9221 + N (DevTools), and 5900 + N (Xvnc, behind the viewer) exist inside the container only.

Every viewer goes through `computerd`, which checks the key, refuses page requests whose `Host` is not `127.0.0.1` or `localhost` on that port, and checks `Origin` on WebSocket upgrades. While a viewer is attached to a screen, the session's idle timer does not run. When a session ends while someone watches, it ends for the agent at once, but its screen stays open until the last viewer disconnects.

The base port is a setting of the computer, read when the computer is created. Two computers on the same machine need different bases, for example `COMPUTER_USE_NAME=other COMPUTER_USE_PORT_BASE=21900`. If a port is taken, `start_computer` says which one and names the setting. A computer that already exists keeps its ports. Remove it with `docker rm` (home stays) to create it again with another base.

## Apps and pages

- `open_path` opens an http(s) URL or a file on the session's screen and returns a screenshot. Pages, and HTML, PDF, image, text, JSON, and XML files, open in the screen's Chromium. Other files open with their default application (`xdg-open`).
- `launch_app` starts an application on the session's screen, or raises its window when one is open, then returns a screenshot. It takes `browser`, `terminal`, the name of an installed application (a `.desktop` entry), or a program on `PATH`. A `uri` is opened in the application, and the browser opens it in a new tab. The `focus` action of `computer_act` does the same when no window matches.
- Every screen has its own Chromium with uBlock Origin Lite, which installs itself from the Chrome Web Store within a minute of the first start. The user can disable it. The Chromium starts the first time something needs a browser on that screen, and closes with the screen so its profile is saved. Each Chromium runs on a fresh profile cloned from a template profile in home.
- Logins are shared across screens. About every 2 seconds `computerd` reads the cookies of every running Chromium over DevTools, merges the changes into one cookie jar in home (`~/.local/share/computer-use/cookies.json`, mode 0600), and writes the differences into the other Chromiums. Logging out spreads the same way. A new Chromium loads the jar before it opens its page. Sign in once on any screen and every other screen, and every screen opened later, is signed in a few seconds after a reload.
- Bookmarks, saved passwords, autofill data, and browser settings (such as page zoom) live in the template profile. A Chromium copies back the ones it changed when it closes cleanly. A running Chromium does not see changes made on another screen until it is closed and the next one starts. When two Chromiums change the same file, the one that closes last wins.
- Known gaps. Logins kept in localStorage or IndexedDB do not carry over. The list of installed extensions and their settings do not carry over, so uBlock installs again at every start. The "Sign in to Chromium" button is cosmetic and does not sync anything. Chromium caps cookie expiry at 400 days, so a longer expiry shows as the capped one. Cookies in opaque partitions stay on their screen. The jar keeps the 5000 most recently changed cookies, and a jar that cannot be read is kept as `cookies.json.bad` before a new one starts.
- The Chromium runs without its own sandbox, because Docker's default seccomp profile blocks the user namespaces that sandbox needs. The container is the boundary. DevTools listens on `127.0.0.1` inside the container only, on port 9221 plus the screen number.
- The right-click menu of a screen offers Browser and Terminal. Both open on that screen. `xdg-open` of a web link inside the computer opens it in that screen's Chromium too.

## Opening the viewer

The viewer opens by itself the first time a session's screen opens, so the user sees the agent work without any step. `COMPUTER_USE_OPEN` picks how, per MCP server process.

- `browser` (default). If a viewer page is already open in a browser, it switches to the new screen and highlights it in the sidebar, and no tab opens. Otherwise a tab opens on the new screen. Several screens opening at once open one tab. The page count is a best guess. If no page is really there, the viewer link from `start_computer` still works.
- `vnc` on macOS runs `open vnc://:<key>@127.0.0.1:<port>`, which opens Screen Sharing.
- `vnc` on Windows runs a VNC viewer directly, because Windows viewers do not read a password from a `vnc://` link. It needs [TigerVNC](https://tigervnc.org). The server uses `COMPUTER_USE_VNC_VIEWER` (the path of `vncviewer.exe`, run as `vncviewer.exe -passwd <file> 127.0.0.1::<port>`), (it must be a file), or else `vncviewer.exe` from `PATH` or from `Program Files\TigerVNC`. The password goes into a private temp file that is removed after 15 s or when the server exits. With no viewer found, the server opens the browser and logs the reason to stderr.
- `none` opens nothing.

Opening never delays or fails a tool call. Problems go to stderr. `vnc` on Linux hosts opens the browser.

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
