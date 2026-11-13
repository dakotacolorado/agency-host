# agency-host

The process that makes a machine available to the workplace. One Node 24
TypeScript package, run on the Claude Agent SDK.

## What it does, and nothing more

Keep a socket open to agency, saying the host is here and what it
offers. Take the commands that come down it: make an agent's folder, start or
stop its session, carry a message to it, delete it. Report what agents do and
say back up it. That is the whole job.

It holds no opinion about what the work is. It does not read tickets, decide
what to run, or know what an agent is for. Anything clever here is in the
wrong place -- it belongs to the agents, or to the service.

The protocol is agency's; its shapes are the authority, and the host
follows them rather than the other way round.

## Writing code here

The code carries the facts; prose carries only what the code cannot. Keep a
comment when it explains a decision the code cannot state -- a constraint, a
trade-off, a reason something is not the obvious way. Delete it when it restates
the line beneath it.

Only `src/sdk.ts` calls the Agent SDK; everything else sees `SessionHandle`,
so tests drive the stand-in in `src/testing.ts`.

The machine this runs on will be closed, suspended, and unplugged without
warning. Anything that assumes it keeps running is wrong.
