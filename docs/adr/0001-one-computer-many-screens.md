# One computer, one screen per session, only home survives

Every agent shares one persistent container instead of getting its own. Each session gets its own screen (X display, window manager, and Chromium) inside it, so parallel agents and subagents never fight over the mouse, while files and installed tools stay shared. Subagents share their parent's MCP connection, so a session comes from an explicit `start_computer` call, not from the connection.

## Consequences

- Only the home volume is guaranteed to survive. The computer is recreated on upgrade, so packages installed with `apt` are lost. Don't add more volumes for system paths: a new image's files would clash with old volume contents.
- Two Chromium processes can't share a profile, so logins are shared through a cookie jar that `computerd` keeps in sync over DevTools. Logins kept in localStorage or IndexedDB don't carry over.
- The server only creates or starts the computer. Recreating it for an upgrade happens only when the user has stopped it.
