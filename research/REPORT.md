# Computer-use MCP research report

Research date: 2026-10-03.

## Scope

This document records the requested behavior as user-stated requirements and describes relevant behavior found in Rakazo and OpenCode documentation. It is research, not an implementation plan. It does not select an architecture, recommend implementation steps, or fill in decisions for the later architecture discussion.

User-stated requirements for that discussion:

- Agents, including OpenCode agents, should be able to use a computer through an MCP server.
- There should be one computer available at a time.
- Each agent should get a view of that computer.
- Computer state should persist across sessions using a Docker volume.
- MCP startup should start a stopped container or create a container if one is absent.
- The MCP should not stop the computer. The user stops or deletes it through Docker.
- The user should have a VNC view for interacting with the computer.
- The MCP should expose computer-control tools.

## Findings at a glance

Rakazo's Docker computer is a Debian Bookworm image with a virtual X display, Fluxbox, Chromium, and a lightweight screen-sharing stack. Its durable home is mounted from host-managed storage. Its supervisor checks for an existing computer and can restart it, but Rakazo also has lifecycle operations that stop or replace computers. The OpenCode documentation describes MCP servers as a way to add model-callable tools. It documents local stdio and remote MCP connections, while the TypeScript MCP SDK supports image content in tool results. The reviewed OpenCode MCP documentation does not document an MCP-defined way to add an embedded VNC panel to the OpenCode interface.

## Rakazo reference examined

The source was cloned from `https://github.com/elie222/rakazo` to a temporary directory for this research. The checked commit was `ce668455` (`Let the desktop guard inspect harmless shell syntax (#1140)`).

### Computer image

The computer image is built from `infra/sandboxes/computer/Dockerfile` and its build context. The base is digest-pinned `debian:bookworm-slim`. The final image installs:

- Xvfb, Fluxbox, xterm, xdotool, wmctrl, X11 utilities, and ImageMagick for a lightweight virtual desktop.
- Chromium as the graphical browser.
- x11vnc, noVNC, and websockify for screen viewing.
- Python 3, Git, `gh`, `uv`/`uvx`, curl, jq, OpenSSH client, unzip, Bash, and clipboard utilities.
- D-Bus and XDG portal packages used by the browser/desktop integration.

The Dockerfile creates and runs as UID/GID 1000, sets the working directory and home to `/home/rakazo`, and uses `/usr/local/bin/rakazo-computer` as its command. The image's Fluxbox menu contains Browser and Terminal. It does not install a full GNOME/KDE desktop environment. The image uses a build stage for the native X capture helper; the compiler and build libraries are not installed in the final stage.

`start.sh` starts the computer control process when `RAKAZO_COMPUTER_CONTROL_TOKEN` is set, then starts Xvfb at `:1` with a 1280x800x24 screen, a D-Bus session, XDG portals, Fluxbox, x11vnc, and noVNC's websockify endpoint. The browser wrapper launches Chromium with a display-specific debugging port and a profile under the persistent home. The page-browser helper connects to that live Chromium through loopback CDP.

The Dockerfile declares ports 7070 and 6080 through 6095. The source documents the screen endpoint as a token-protected gateway and the computer control API on 7070. Port publication, network mode, and control publication are supervisor settings; the image's `EXPOSE` lines alone do not publish ports.

### Persistence and lifecycle in Rakazo

The supervisor's default image name is `rakazo/computer:local`, overridden by `RAKAZO_COMPUTER_IMAGE`. If that image is not present, the supervisor attempts to build it from `infra/sandboxes/computer`. This is in `infra/sandboxes/supervisor/src/computer-spec.ts` and `src/index.ts`.

For the Docker computer, `computerHomeStorage()` locates the home below the supervisor's data directory. If the data directory is a Docker volume, the home is mounted at `/home/rakazo` using a volume subpath. Otherwise the supervisor binds the corresponding host directory. The container configuration has `AutoRemove: false`, so container removal is not coupled to process exit. The supervisor looks up an existing container and starts it again when its image, user, network, control port, and home-volume configuration still match.

Rakazo also contains stop, destroy, idle-shutdown, replacement, and recovery paths. The documented persistence boundary is the workspace/home, not the container's writable layer. The OS image itself is disposable. Thus the observed Rakazo lifecycle differs from the user-stated condition that only the user stops the computer.

The runtime guide describes Team computers as sharing files and installed tools while active team bots receive distinct displays and browser processes/profiles. The supervisor container name is derived from a bot ID. These are source facts from different parts of the runtime; their exact relationship to a single-container MCP setup is not assumed here.

The current computer container configuration sets defaults of 2 GiB memory, 2 CPUs, 2048 pids, and 256 MiB shared memory. It drops Linux capabilities and enables `no-new-privileges`. The image's Chrome launcher includes `--no-sandbox`. The runtime guide states that team bots share the OS user, workspace, browser profiles, and shell/X11 access, so leases coordinate tool calls but are not isolation between mutually untrusted processes.

### Rakazo computer tools

The model-facing desktop surface is in `packages/adapters/src/builtin-tools.ts` and the implementations/adapters are in `packages/adapters/src/computer-tools.ts`, `browser-tools.ts`, and `computer-browser.ts`.

- `computer_observe` returns a screenshot plus frame and active-window metadata.
- `computer_act` accepts up to 24 ordered click, move, button, typing, key, scroll, wait, and app-focus actions, then returns an observation.
- `browser_navigate`, `browser_snapshot`, and `browser_act` operate on the visible Chromium page through CDP. Browser actions use refs from a page snapshot. Unsupported/unavailable page operations report a desktop-control fallback.
- `shell`, `list_files`, `read_file`, and `write_file` provide command and workspace operations.
- `open_path` and `launch_app` open a file, URL, or installed application graphically.

Rakazo's `docs/computer-runtime.md` says that the Pi agent runtime runs in the API/worker process, not in the computer container. Its provider contract separates provisioning, command execution, files, screen observation, actions, and screen sessions.

### Source issue to keep visible during later discussion

At the checked commit, the supervisor's Docker build request in `infra/sandboxes/supervisor/src/index.ts` supplies an explicit list of build-context files. That list does not include `rakazo-focus-or-launch` or `rakazo-local-bin.sh`, while the computer Dockerfile has `COPY` instructions for both. This report did not run a Docker build, so it records the file-list mismatch only and does not claim a runtime outcome.

## OpenCode and MCP facts

OpenCode's MCP documentation says MCP tools are automatically available to the LLM alongside built-in tools. It documents:

- Local MCP servers configured with a command and optional environment.
- Remote MCP servers configured with a URL and optional headers, OAuth, enable flag, and timeout.
- MCP management commands such as `opencode mcp add`, `list`, `auth`, and `logout`.

The official TypeScript MCP SDK documentation describes stdio for local process-spawned integrations and Streamable HTTP for remote servers. It states that tool results may contain image content as base64 data plus a MIME type. This provides a protocol-level way to return screenshots; it does not by itself establish how an OpenCode interface would render a continuously interactive VNC view.

OpenCode's plugin documentation describes plugin hooks and custom tools. The reviewed MCP and plugin documentation does not specify an MCP tool that can mount or add an embedded VNC view to the OpenCode UI. Whether the desired VNC section is host UI behavior, a separate local web view, an OpenCode plugin, or another surface remains an open architecture decision.

References:

- OpenCode MCP servers: <https://opencode.ai/docs/mcp-servers>
- OpenCode plugins: <https://opencode.ai/docs/plugins>
- MCP TypeScript SDK server documentation: <https://ts.sdk.modelcontextprotocol.io/server.html>

The summary of OpenCode behavior is copied into `references/opencode/MCP-HOST-FACTS.md`.

## Facts versus unresolved questions

The observations above describe the checked Rakazo source and the cited OpenCode/MCP documentation. The following points are intentionally left for the later architecture discussion:

- Whether “one computer” means one persistent container/display shared concurrently, or one container with multiple agent sessions taking turns.
- Whether an agent view is a screenshot returned to the model, an interactive user VNC view, or both.
- How the OpenCode UI should present the user's VNC view. The reviewed MCP documentation describes tools, not a built-in VNC panel.
- Which agents may connect, how simultaneous tool calls are serialized, and what happens when a user interacts while an agent is active.
- Which Docker volume layout and container naming/lifecycle rules apply.
- Which host/network bindings and authentication controls apply to MCP and VNC access.
- Whether to reuse Rakazo's image/build context as-is or choose a different image. No choice is made here.

## Copied reference material

`references/rakazo-computer-image/` contains the full source build context from Rakazo's `infra/sandboxes/computer/`, including the Dockerfile, startup/control code, browser helpers, Fluxbox configuration, and tests.

`references/rakazo-runtime/` contains selected supervisor, provider-interface, tool-adapter, and runtime documentation source files.

`references/opencode/` contains a local MCP-host fact summary with official documentation links.

## Actual Docker image artifact status

The image binary could not be exported in this environment. The Docker CLI was present, but its Docker Desktop Linux engine pipe was unavailable, so `docker image ls` could not connect to a daemon. The Rakazo source identifies `rakazo/computer:local` as the default image name and describes building that image from the copied Dockerfile/build context. No image archive (`docker save` output) was found in the cloned source tree. See `references/IMAGE-ARTIFACT-STATUS.md`.
