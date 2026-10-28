#!/bin/sh
# Proves the built binary and the service agree on a whole cycle, with the
# routes the service serves, both ways a command reaches a host:
#
#   by heartbeat: the host registers by sending one; a person creates a
#     worker on it; the host takes the create from its next heartbeat's reply,
#     makes the worker's folder and reports it.
#   by socket: the host runs on, its socket open; a person messages the idle
#     worker, which starts a session, and then stops it; each command is
#     pushed, taken and reported well inside one 30 s heartbeat.
#   by heartbeat again: the person retires the worker and the host reports it.
#
#   WORKER_HOST_SERVICE_URL=… WORKER_HOST_TOKEN=… WORKER_HOST_ID=… heartbeat-cycle.sh BINARY
#
# Give it an id no run has used before. Its worker is left retired -- however
# the cycle ends -- and its host deregistered. The worker's Claude Code is a
# stand-in, under tmux on a socket, config and folders of the run's own, so
# nothing a real host on the same machine has is read or touched.
#
# Writes proved=true|false to $GITHUB_OUTPUT. No service to send heartbeats to
# -- no url, nothing answering at it, no heartbeat served there yet, or no
# socket named by it yet -- is a dependency still to come, not a failure: it
# ends green with proved=false, and nothing is released on it. Anything else
# that goes wrong fails.
set -eu

bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
url=${WORKER_HOST_SERVICE_URL:-}
url=${url%/}
host=$WORKER_HOST_ID
scratch=$(mktemp -d)
# tmux's socket path must fit in 104 bytes, and macOS's $TMPDIR alone is half that.
sockets=$(mktemp -d /tmp/wh.XXXXXX)
log=$scratch/log
out=$scratch/out
worker=
running=

# A heartbeat is 30 s apart, so a command done sooner came by the socket.
pushed=10

proved() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "proved=$1" >> "$GITHUB_OUTPUT"; fi
}
not_yet() {
  echo "::warning::$1; the cycle is unproved and nothing will be released"
  proved false
  exit 0
}
fail() {
  echo "::error::$1"
  exit 1
}

[ -n "$url" ] || not_yet "skipped: waiting for a beta service url to send heartbeats to; none was given"
# Any HTTP answer at all, even an error, means something is there to send
# heartbeats to.
if [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$url/" || true)" = 000 ]; then
  not_yet "skipped: waiting for the beta service at $url to answer; nothing did within 10s"
fi
auth="Authorization: Bearer $WORKER_HOST_TOKEN"

# What a person does, through the same routes: the body lands in $out, the
# status is printed.
api() {
  curl -s -o "$out" -w '%{http_code}' -X "$1" -H "$auth" -H 'Content-Type: application/json' \
    ${3:+-d "$3"} "$url$2"
}
state() {
  [ "$(api GET "/fleet/workers/$worker")" = 200 ] || fail "reading worker $worker: $(cat "$out")"
  jq -r '(.worker // .) | .state' "$out"
}
# The id of the command of `kind` the service sent for the worker, and how its
# delivery stands.
command_of() {
  [ "$(api GET "/fleet/workers/$worker/commands")" = 200 ] || fail "listing $worker's commands: $(cat "$out")"
  jq -r --arg kind "$1" '[.commands[] | select((.command // .).kind == $kind)] | last
    | "\((.command // .).id) \(.delivery)"' "$out"
}
# Waits up to $pushed seconds for `kind`'s command to be done and the worker
# to be in `want`, as only the socket can manage.
pushed_until() {
  kind=$1 want=$2 waited=0
  while :; do
    set -- $(command_of "$kind")
    now=$(state)
    if [ "$2" = done ] && [ "$now" = "$want" ]; then
      echo "$kind $1 done by push in under ${pushed}s, and the worker is $now"
      return
    fi
    [ "$waited" -lt $((pushed * 4)) ] ||
      fail "$kind $1 is '$2' and the worker '$now' after ${pushed}s, not done and '$want': the socket did not carry it"
    waited=$((waited + 1))
    sleep 0.25
  done
}

mkdir -p "$scratch/bin" "$scratch/home" "$scratch/tmux"
: > "$scratch/config.toml"
printf '#!/bin/sh\nexec sleep 600\n' > "$scratch/bin/claude"
chmod +x "$scratch/bin/claude"
export WORKER_HOST_CONFIG="$scratch/config.toml" WORKER_HOST_TMUX_SOCKET="$host" WORKER_HOST_CAPACITY=1
export WORKER_HOST_WORKERS_DIR="$scratch/home/workers" TMUX_TMPDIR="$sockets" PATH="$scratch/bin:$PATH"
unset WORKER_HOST_LABELS

cleanup() {
  if [ -n "$running" ]; then kill "$running" 2> /dev/null || true; fi
  if [ -n "$worker" ] && [ "$(state 2> /dev/null)" != retired ]; then
    api DELETE "/fleet/workers/$worker" > /dev/null || true
    echo "retired $worker, which the cycle left unretired"
  fi
  api DELETE "/fleet/hosts/$host" > /dev/null || true
  tmux -L "$host" kill-server 2> /dev/null || true
  rm -rf "$scratch" "$sockets"
}
trap cleanup EXIT

heartbeat() {
  if ! "$bin" --heartbeats 1 2> "$log"; then
    cat "$log"
    grep -q '^heartbeat failed: 404 ' "$log" && not_yet "the service does not serve POST /fleet/hosts/{id}/heartbeat yet"
    fail "a heartbeat did not land"
  fi
  cat "$log"
}

heartbeat
echo "registered $host"

name=$(echo "ci-$host" | tr -c 'a-z0-9-\n' '-' | cut -c1-40)
status=$(api POST /fleet/workers "{\"name\":\"$name\",\"hostId\":\"$host\"}")
[ "$status" = 201 ] || fail "creating a worker on $host: $status $(cat "$out")"
worker=$(jq -er '(.worker // .) | .id' "$out")
echo "created $worker"

# One heartbeat's reply carries the create, and the heartbeat after it the
# report.
heartbeat
set -- $(command_of create)
grep -q "^command $1 (create): done$" "$log" || fail "the host did not take create $1 from its heartbeat"
[ "$2" = done ] || fail "create $1 is $2, not done: $(cat "$out")"
[ -f "$WORKER_HOST_WORKERS_DIR/$name/worker.json" ] || fail "create $1 made no folder for $name"
[ "$(state)" = idle ] || fail "worker $worker is '$(state)' once created, not 'idle'"
echo "created $worker by heartbeat: create $1 reported done, and the worker is idle"

"$bin" 2> "$scratch/running.log" &
running=$!
waited=0
until grep -q '^socket open: ' "$scratch/running.log"; do
  kill -0 "$running" 2> /dev/null || fail "the host stopped: $(cat "$scratch/running.log")"
  if [ "$waited" -ge 60 ]; then
    cat "$scratch/running.log"
    grep -q '^socket could not open: ' "$scratch/running.log" && fail "the host could not open the socket the service named"
    not_yet "the service names no socket in its heartbeat's reply yet"
  fi
  waited=$((waited + 1))
  sleep 0.5
done
echo "socket open"

status=$(api POST "/fleet/workers/$worker/messages" '{"text":"A stand-in, created by worker-host CI."}')
[ "$status" = 202 ] || fail "messaging $worker: $status $(cat "$out")"
pushed_until start working

status=$(api POST "/fleet/workers/$worker/stop" '{}')
[ "$status" = 202 ] || fail "stopping $worker: $status $(cat "$out")"
pushed_until stop idle

kill "$running"
wait "$running" 2> /dev/null || true
running=
cat "$scratch/running.log"

status=$(api DELETE "/fleet/workers/$worker")
[ "$status" = 202 ] || fail "retiring $worker: $status $(cat "$out")"
heartbeat
set -- $(command_of retire)
grep -q "^command $1 (retire): done$" "$log" || fail "the host did not take retire $1 from its heartbeat"
[ "$2" = done ] || fail "retire $1 is $2, not done"
[ "$(state)" = retired ] || fail "worker $worker is '$(state)' once retired, not 'retired'"
echo "retired $worker by heartbeat: retire $1 reported done"

proved true
