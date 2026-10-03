# How Rakazo captures `computer_observe`

For Rakazo's Docker computer, `computer_observe` captures the virtual X11 desktop directly. It does not take the screenshot through VNC.

1. The agent tool handler calls the sandbox provider's `observe()` method. The handler is in Rakazo's `packages/adapters/src/executor.ts`; the copied result formatter is [`computer-tools.ts`](references/rakazo-runtime/computer-tools.ts#L90).
2. The supervisor checks the computer and screen lease, then asks the computer control service for an observation with no actions and `observe: true`. See [`supervisor-index.ts`](references/rakazo-runtime/supervisor-index.ts#L529).
3. Inside the container, `control.py` captures the X display's root window. Its fast path calls Rakazo's native capture library. That library uses X11 shared memory (`XShm`) to copy screen pixels, converts them to RGB, and encodes them as PNG with libpng. See [`control.py`](references/rakazo-computer-image/control.py#L48) and [`xcapture.c`](references/rakazo-computer-image/xcapture.c#L100).
4. The result includes the PNG and metadata such as screen size, cursor position, and active-window title. XDamage can also report the changed screen region.
5. Rakazo returns the image to the agent as image content alongside text metadata. If it has the same frame as the previous observation, it can omit the repeated image data. See [`computer-tools.ts`](references/rakazo-runtime/computer-tools.ts#L90).

If native capture is unavailable, `control.py` falls back to ImageMagick's `import -window root`. If the supervisor's control-service path is unavailable, the supervisor has another fallback that runs ImageMagick inside the container. See [`control.py`](references/rakazo-computer-image/control.py#L255) and [`supervisor-index.ts`](references/rakazo-runtime/supervisor-index.ts#L1673).

The VNC/noVNC services are separate. They let a person view the same desktop, but they are not the screenshot-capture mechanism used by `computer_observe`.
