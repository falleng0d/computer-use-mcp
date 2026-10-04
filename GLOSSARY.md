# Computer use

An MCP server that gives agents one persistent Linux desktop they can see and control, which the user can also watch and use.

## Language

**Computer**:
The single persistent Linux machine that agents and the user share. Only the user stops or deletes it.
_Avoid_: Sandbox, VM, machine, box, container

**Home**:
The part of the computer that is guaranteed to survive, even when the computer is deleted and created again. Everything outside it may be lost then.
_Avoid_: Workspace, volume, data dir

**Screen**:
One desktop inside the computer, with its own windows, mouse, keyboard focus, and browser profile. Every screen sees the same files.
_Avoid_: View, display, desktop

**Session**:
One agent's stay on the computer, named after the agent's task. A session has at most one screen, and ends when its agent leaves or goes idle.
_Avoid_: Lease, slot, connection

**Agent**:
Anything that calls the computer's tools. Each agent works in its own session, even when several agents share one MCP connection.
_Avoid_: Bot, client

**User**:
The person who owns the computer, watches and uses screens through VNC, and alone stops or deletes it.
_Avoid_: Operator, owner, human
