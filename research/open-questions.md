# Open questions for the desktop stack

Checked on 2026-10-03. The measured checks ran in a throwaway `debian:trixie-slim` container (amd64, Docker 29.5.2, default `--ipc`, 64 MB `/dev/shm`), which was removed afterwards. Labels: **measured** (ran it), **documented** (primary source), **inferred**.

## 1. Xvnc extensions and resizing

Yes. `tigervnc-standalone-server` 1.15.0+dfsg-2.1~deb13u1 offers MIT-SHM, DAMAGE, XTEST, RANDR, XFIXES, and Composite. **Measured.**

- `xdpyinfo` lists 24 extensions, including MIT-SHM 1.2 with shared pixmaps.
- `xrandr -s 1280x800` works. The output `VNC-0` has a preset mode list, and `--newmode`/`--addmode`/`--output VNC-0 --mode` adds any other size (tested 1500x900 and 7680x4320). The maximum is 32768x32768.
- `xrandr --fb W×H` returns BadValue when the new size is smaller than the current output. Use `-s` or a mode instead.
- `-AcceptSetDesktopSize=0` works (the default is on). A small RFB client sent SetDesktopSize 800x600. With the flag the screen stayed at 1500x900. Without it the screen changed to 800x600.

## 2. MIT-SHM capture across processes

Yes, with no `--ipc` flag. **Measured.**

- A second process attached a SysV segment and called `XShmGetImage` on the root window of `:1`. All calls returned success, and the centre pixel matched the `xsetroot` colour.
- A 7680x4320 capture used a 132 MB segment while `/dev/shm` was 64 MB, and `/dev/shm` stayed at 0% use. SysV segments live in the container's IPC namespace, not in `/dev/shm`, so the 64 MB default does not limit them. `shmmax` and `shmall` are unlimited.
- `/dev/shm` does matter for Chromium, which uses it for shared memory. Give the container `--shm-size` of 1 GB or more, or start Chromium with `--disable-dev-shm-usage`. **Inferred** (standard Chromium-in-Docker advice, not measured here).
- MIT-SHM 1.2 fd-passing (`CreateSegment`/`AttachFd` in x11rb) was not tested. **Inferred** to work the same way.

## 3. Chromium `ExtensionSettings` policy

Yes. Debian Chromium 154.0.8037.92-1~deb13u1 reads `/etc/chromium/policies/managed/*.json` and installs uBlock Origin Lite from the Web Store. **Measured.**

- Within 60 seconds of the first start, `Default/Extensions/ddkjiahejlhfcafbddmgiahcphecmpfh/2026.930.1227_0/` existed. `chrome.management` reported "uBlock Origin Lite" as enabled.
- `normal_installed` gives `installType: "sideload"` and `mayDisable: true`. `chrome.management.setEnabled(id, false)` worked, so the user can disable it.
- `force_installed` gives `installType: "admin"` and `mayDisable: false`. The same call failed with "cannot be modified by user", and the extension stayed enabled.
- Use `force_installed` if users must not turn it off.

## 4. Chrome Sync in third-party Chromium

Yes. Google announced on 2021-01-15 that, from 2021-03-15, third-party Chromium builds lose access to Chrome Sync and other private Google APIs (Click to Call, spelling, translate element, geolocation, contacts). Debian's Chromium is one of these builds, so it cannot use Chrome Sync. That is why we sync cookies ourselves. **Documented** in [Limiting Private API availability in Chromium](https://blog.chromium.org/2021/01/limiting-private-api-availability-in.html) (Chromium blog, Jochen Eisinger).

## 5. Cookie sync over CDP

Use `Storage.getCookies` and `Storage.setCookies` on the browser target. Neither is experimental or deprecated. There is no cookie-change event, so the daemon must poll. **Measured** on Chromium 154 through `/json/protocol` and a live round trip, and matches the [protocol JSON](https://github.com/ChromeDevTools/devtools-protocol/blob/master/json/browser_protocol.json).

- `Storage.getCookies({browserContextId?})` returns every cookie. Each entry has `httpOnly`, `secure`, `session`, `sameSite`, `expires`, `priority`, `sourceScheme`, `sourcePort`, `partitionKey {topLevelSite, hasCrossSiteAncestor}`, and `partitionKeyOpaque`.
- A test set a session HttpOnly SameSite=Strict cookie, a persistent Lax cookie, and a partitioned (CHIPS) cookie. It read them, cleared the store, and wrote them back with `Storage.setCookies`. The second read matched the first on every field above, including `partitionKey`.
- Chromium caps `expires` at 400 days on write. A cookie set to expire in 2033 came back with an expiry in 2027.
- `Network.getAllCookies` is deprecated ("Use Storage.getCookies instead"). It returned "wasn't found" on the browser endpoint. `Network.getCookies`, `Network.setCookie(s)`, and `Network.deleteCookies` still exist, but they act on a page target.
- The only events that mention cookies are `Network.requestWillBeSentExtraInfo` and `Network.responseReceivedExtraInfo`. They fire per page, per request, and they miss cookies set by `document.cookie`. Polling `Storage.getCookies` and comparing results is the reliable method.

## 6. `vnc://` handlers on Windows and macOS

On Windows, none of the common viewers both registers `vnc://` and accepts a password in the URL. The host binary should run the viewer directly, or pass a password file, instead of relying on `vnc://`.

- **TigerVNC viewer** does not parse URLs. It accepts only `host:display`, `host::port`, or a `.tigervnc` file, and its Windows installer registers no URL handler. Issue [#1229](https://github.com/TigerVNC/tigervnc/issues/1229) asks for URL support and is still open. **Documented** in the [vncviewer manual](https://tigervnc.org/doc/vncviewer.html). It accepts `-passwd <file>`.
- **RealVNC Viewer** installs a URI handler, but RealVNC says it accepts "no additional parameters other than the RealVNC Server address", so a URL cannot carry a password. **Documented** in the [RealVNC help article](https://help.realvnc.com/hc/en-us/articles/6449870411037). Whether the Windows scheme is `vnc://` or `com.realvnc.vncviewer.connect://` was not confirmed.
- **TightVNC**: its installer associates only `.vnc` files. A `vnc://` feature request ([#242](https://sourceforge.net/p/vnc-tight/feature-requests/242/)) was never built. **Documented.**
- **UltraVNC**: no release notes up to 1.8.3.0 mention a `vnc://` handler. Users add one by hand in the registry. **Documented** (by absence) on the [uvnc.com release pages](https://uvnc.com/downloads/ultravnc/172-ultravnc-1-8-3-0.html).
- **macOS Screen Sharing** handles `vnc://` through `open`. It connects to non-Apple servers with classic VNC password auth (RFB security type 2), and `vnc://:password@host:port` fills in the password. **Inferred** from common use, not tested on a Mac. Two cautions, also inferred. First, Xvnc must offer `VncAuth`, because Screen Sharing does not speak TigerVNC's default VeNCrypt types and handles `None` badly. Second, the password stays in shell history and process lists.

## 7. Tool installs on trixie (amd64 and arm64)

All **documented**.

- **Node.js LTS**: NodeSource `curl -fsSL https://deb.nodesource.com/setup_24.x | bash - && apt-get install -y nodejs`. Its `nodistro` suite covers amd64 and arm64. Pin the major version rather than using `setup_lts.x`, which moves. Trixie's own `nodejs` is 20.x and end of life. [nodesource/distributions](https://github.com/nodesource/distributions)
- **AWS CLI v2**: `https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip` (`x86_64` or `aarch64`), then `unzip` and `./aws/install`. Verify with the `.sig` file and GPG key `FB5D…475C`. Needs `unzip`, `groff`, and `less`. [AWS install guide](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html)
- **gh**: add the key from `https://cli.github.com/packages/githubcli-archive-keyring.gpg` to `/etc/apt/keyrings/`, then `deb [arch=$(dpkg --print-architecture) signed-by=…] https://cli.github.com/packages stable main`. [cli/cli install_linux.md](https://github.com/cli/cli/blob/trunk/docs/install_linux.md)
- **uv**: `COPY --from=ghcr.io/astral-sh/uv:<version> /uv /uvx /bin/`, pinned to a version or a digest. The image is multi-arch. [uv Docker guide](https://docs.astral.sh/uv/guides/integration/docker/)
