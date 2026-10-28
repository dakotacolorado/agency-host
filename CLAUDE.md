# worker-host

The process that makes a machine available to the workplace. One crate, one
static binary per architecture, no runtime to install.

## What it does, and nothing more

Send heartbeats to worker-service saying which workers are here and what
they are doing, and keep its socket open. Take the commands that come either way: make a worker's
folder, start or stop its session, carry a message to it, retire it. Report
what workers do and say back out. That is the whole job.

It holds no opinion about what the work is. It does not read tickets, decide
what to run, or know what a worker is for. Anything clever here is in the
wrong place -- it belongs to the workers, or to the service.

The protocol is worker-service's; its shapes are the authority, and the host
follows them rather than the other way round.

## Writing code here

The code carries the facts; prose carries only what the code cannot. Keep a
comment when it explains a decision the code cannot state -- a constraint, a
trade-off, a reason something is not the obvious way. Delete it when it restates
the line beneath it.

The machine this runs on will be closed, suspended, and unplugged without
warning. Anything that assumes it keeps running is wrong.
