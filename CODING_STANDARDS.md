# Coding standards

These are the rules that matter most for this project. They are not a full Rust style guide. `rustfmt` and Clippy settle everything they can check, and `just check` must pass before every push.

## Tooling

- `just check` runs `cargo fmt --check`, Clippy with `-D warnings`, and the tests. CI runs the same recipe.
- Clippy runs the `all` and `pedantic` groups, plus `unwrap_used`, `dbg_macro`, `undocumented_unsafe_blocks`, and `allow_attributes`. Fix the code rather than silencing the lint.
- When a lint is wrong for one spot, use `#[expect(clippy::lint_name, reason = "...")]`, never `#[allow]`. `expect` fails once the exception is no longer needed.
- Add dependencies with `cargo add`. Every new dependency needs a reason. Prefer crates that are widely used and maintained. Turn off default features you don't need. Use `rustls` rather than OpenSSL, so Windows and macOS builds need no system libraries.

## Project rules

- **stdout belongs to MCP.** The host binary speaks MCP over stdio. Anything else written to stdout breaks the client. Logs and diagnostics go to stderr. Only `info`, `--help`, and `--version` print to stdout.
- **The MCP server never stops the computer, and removes it in two cases only.** It may create or start the container. Stopping and deleting it is the user's job. The one removal is in `Docked::recreate`: a container that is stopped, re-inspected as still stopped under the same ID, and on an image older than the wanted one (`upgrade::is_newer`). It is removed by ID without force, then created again with the same name, volume, and port base. The other is a container the same call just created that could not publish its ports. Never remove a running container, never move to an older or unordered image, and never add another removal.
- **The host binary must build and run on Windows and macOS.** Use `std::path` and `PathBuf`, never hand-built path strings. Keep Linux-only code (X11, `/proc`) in `computerd`.
- **Shared wire types live in `computer-protocol`.** Bump `PROTOCOL_VERSION` on any breaking change to a request or response. Get the version string from `computer_protocol::VERSION`, never from `CARGO_PKG_VERSION`.
- **Every call that leaves the process has a timeout.** That includes the Docker API, HTTP to `computerd`, and child processes. A hung call must turn into an error the agent can see.

## Design

- **Make wrong states hard to write.** Use enums and newtypes rather than `bool` flags, bare strings, or several `Option` fields that must be set together. Parse input into typed values at the edge (MCP arguments, HTTP bodies, env vars), then pass the typed values inward.
- **Keep logic apart from I/O.** Decisions should live in plain functions that take values and return values. Thin outer functions read the environment, call Docker, or touch the screen. `image::select` and `image::from_env` show the split. The plain part is what you test.
- **Keep visibility small.** Default to private, then `pub(crate)`. Use `pub` only for what another crate uses.
- **Use short, plain names.** Avoid filler words like `Manager`, `Service`, `Helper`, or `Util`. Name a function after what it returns or does.
- **Name your constants.** Ports, timeouts, sizes, and image names get a `const` with a clear name.
- **Write the simple version first.** Clone instead of fighting lifetimes until profiling shows the clone matters. Screen capture and input are the hot paths. Measure them before and after any change made for speed.
- **Avoid wildcard imports**, except `use super::*;` in test modules.

## Errors and panics

- Binaries return `anyhow::Result` and add `.context("...")` at each step a reader needs to understand the failure. Say what was being attempted, for example `.context("starting the computer container")`.
- Define an error enum with `thiserror` only when a caller branches on the kind of failure. Don't create one variant per failing call.
- Report errors to agents as MCP tool errors with a message the agent can act on. Don't crash the server because one tool call failed.
- Log an error where it is handled, not where it is passed up with `?`.
- A panic means a bug. Never use `unwrap()` outside tests. Use `expect("...")` only for invariants the code itself guarantees, and state the invariant in the message.

## Async and concurrency

- Async code must reach an `.await` often, roughly every 100 µs. Run blocking work with `tokio::task::spawn_blocking`. That includes file I/O, X11 calls, PNG encoding, and synchronous libraries.
- Never hold a lock across `.await`. A `std::sync::Mutex` is fine for short sections with no `.await`. Use `tokio::sync::Mutex` only when the guard must live across an `.await`.
- Every spawned task has an owner that awaits it or cancels it. Don't spawn tasks and forget them.
- Long-running processes handle Ctrl+C and SIGTERM and shut down cleanly.

## Unsafe code

- Avoid `unsafe`. When it is needed (for example X11 shared memory), keep it in one small module with a safe API.
- Every `unsafe` block gets a `// SAFETY:` comment that explains why the call is sound. Clippy enforces this.

## Logging

- Use `tracing` with structured fields, for example `info!(container = %name, "started computer")`, not values formatted into the message.
- Never log tokens or passwords. The one exception is that the container prints its VNC link and credentials on purpose, so the user can reconnect.

## Comments and docs

- Write no comments by default. Add one only when a future editor would otherwise break something, for example an outside constraint or an invariant the code can't show. One line is the goal.
- A comment describes the code as it is. Never describe a change, a fix, or what the code used to do. That belongs in the commit message.
- Public items in `computer-protocol` get a doc comment whose first sentence is one short line. Doc comments say what callers need to know, such as errors, panics, and units. They don't repeat the signature.

## Testing

A test earns its place only if it would fail when the behavior it protects breaks. Fewer strong tests beat many weak ones.

**Before adding a test, answer these questions:**
1. Which behavior does it protect?
2. Which realistic bug would make it fail?
3. Does the assertion tell the correct result apart from that bug?
4. Does it run the real production code, not a copy?
5. Is it the only test that catches that bug?

If any answer is missing, don't add the test.

**Where tests go:**
- Unit tests sit at the bottom of the file they test, in `#[cfg(test)] mod tests`. Our crates are internal, so tests may use private items.
- If a crate ever needs integration tests, put them all in one binary at `tests/it/main.rs` with submodules. Each file in `tests/` becomes a separate binary, which slows builds.
- Tests that need Docker or a live X display are marked `#[ignore = "needs Docker"]` and run with `cargo test -- --ignored`. They use unique container and volume names, and they clean up even when they fail.

**How to write tests:**
- Name the test after the outcome, for example `dev_build_uses_local_image_without_pulling`, not `test_select`.
- Compare whole values with `assert_eq!` rather than checking pieces. Use `matches!` for error kinds. Check message text only when the text is the contract, such as an error shown to the agent.
- Keep tests deterministic. Don't use real sleeps or the network. For time-based code, use `#[tokio::test(start_paused = true)]`, which needs tokio's `test-util` feature in dev-dependencies.
- Use real code wherever practical. Fake only slow or unsafe boundaries, such as the Docker daemon and the X server, and make fakes reject inputs the real thing would reject.
- Don't test what the compiler, Clippy, or serde derive already guarantees.
- Every reproducible bug fix comes with a test that fails without the fix.
- Delete tests that no longer protect anything. Test count and coverage are not goals.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org). release-plz reads them to pick the next version and write the changelog, so the type matters. A breaking change (`feat!:`) gives the biggest bump, then `feat:`. Any other commit that changes a crate's files gives a patch bump. See the table in `RELEASE.md`.

## Sources

- [Rust API Guidelines checklist](https://rust-lang.github.io/api-guidelines/checklist.html)
- [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
- [Effective Rust](https://effective-rust.com/) by David Drysdale
- [Async: What is blocking?](https://ryhl.io/blog/async-what-is-blocking/) by Alice Ryhl
- [Error handling in Rust](https://www.lpalmieri.com/posts/error-handling-rust/) by Luca Palmieri
- [Delete Cargo Integration Tests](https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html) by Alex Kladov
