# agent-host

The process that makes a machine available to the workplace. One crate, one
static binary per architecture, no runtime to install.

## What it does, and nothing more

Beat to the service with what this machine looks like. Take the commands that
come back: start an agent, stop one, carry a message to one. Stream what agents
say back out. That is the whole job.

It holds no opinion about what the work is. It does not read tickets, decide
what to run, or know what an agent is for. Anything clever here is in the wrong
place -- it belongs to the agents, or to the service, or to the tracker.

## Writing code here

The code carries the facts; prose carries only what the code cannot. Keep a
comment when it explains a decision the code cannot state -- a constraint, a
trade-off, a reason something is not the obvious way. Delete it when it restates
the line beneath it.

The machine this runs on will be closed, suspended, and unplugged without
warning. Anything that assumes it keeps running is wrong.
