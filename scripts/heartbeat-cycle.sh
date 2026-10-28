#!/bin/sh
# Proves the built binary and the service agree on a whole heartbeat cycle, with
# the routes the service serves: the host registers by sending a heartbeat, a
# person places an agent on it, the host takes the start from its next
# heartbeat's reply, runs it and reports it, a person stops it, and the host
# takes and reports the stop.
#
#   AGENT_HOST_SERVICE_URL=… AGENT_HOST_TOKEN=… AGENT_HOST_ID=… heartbeat-cycle.sh BINARY
#
# Give it an id no run has used before. Its agent is left stopped -- or,
# should the cycle break first, force-stopped -- so nothing it placed stays
# active. The agent is a stand-in for Claude Code, under tmux on a
# socket, config and folders of the run's own, so nothing a real host on the
# same machine has is read or touched.
#
# Writes proved=true|false to $GITHUB_OUTPUT. No service to send heartbeats to
# -- no url, nothing answering at it, or no heartbeat served there yet -- is a
# dependency still to come, not a failure: it ends green with proved=false, and
# nothing is released on it. Anything else that goes wrong fails.
set -eu

bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
url=${AGENT_HOST_SERVICE_URL:-}
url=${url%/}
host=$AGENT_HOST_ID
scratch=$(mktemp -d)
log=$scratch/log
out=$scratch/out
agent=

proved() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "proved=$1" >> "$GITHUB_OUTPUT"; fi
}
not_yet() {
  echo "::warning::$1; the heartbeat cycle is unproved and nothing will be released"
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
auth="Authorization: Bearer $AGENT_HOST_TOKEN"

# What a person does, through the same routes: the body lands in $out, the
# status is printed.
api() {
  curl -s -o "$out" -w '%{http_code}' -X "$1" -H "$auth" -H 'Content-Type: application/json' \
    ${3:+-d "$3"} "$url$2"
}
agent_is() {
  [ "$(api GET "/fleet/agents/$agent")" = 200 ] || fail "reading agent $agent: $(cat "$out")"
  jq -r '(.agent // .) | "\(.state) \(.active)"' "$out"
}

mkdir -p "$scratch/bin" "$scratch/data" "$scratch/claude"
: > "$scratch/config.toml"
printf '#!/bin/sh\nexec sleep 600\n' > "$scratch/bin/claude"
chmod +x "$scratch/bin/claude"
export AGENT_HOST_CONFIG="$scratch/config.toml" AGENT_HOST_TMUX_SOCKET="$host" AGENT_HOST_CAPACITY=1
export XDG_DATA_HOME="$scratch/data" CLAUDE_CONFIG_DIR="$scratch/claude" PATH="$scratch/bin:$PATH"
unset AGENT_HOST_LABELS AGENT_HOST_REPOSITORIES

cleanup() {
  if [ -n "$agent" ] && [ "$(agent_is 2> /dev/null | cut -d' ' -f2)" = true ]; then
    api POST "/fleet/agents/$agent/stop" '{"force":true}' > /dev/null || true
    echo "force-stopped $agent, which was still active"
  fi
  api DELETE "/fleet/hosts/$host" > /dev/null || true
  tmux -L "$host" kill-server 2> /dev/null || true
  rm -rf "$scratch"
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

status=$(api POST /fleet/agents "{\"hostId\":\"$host\",\"workItemId\":null,\"role\":\"ci\",\
\"createdBy\":null,\"brief\":\"A stand-in, placed by agent-host CI to prove a heartbeat cycle.\"}")
[ "$status" = 201 ] || fail "placing an agent on $host: $status $(cat "$out")"
agent=$(jq -er .id "$out")
echo "placed $agent"

# One heartbeat's reply carries the start, and the heartbeat after it the
# report.
heartbeat
[ "$(api GET "/fleet/agents/$agent/commands")" = 200 ] || fail "listing $agent's commands: $(cat "$out")"
start=$(jq -er '.commands[] | select(.command.kind == "start") | .command.id' "$out")
grep -q "^command $start (start): done$" "$log" || fail "the host did not take start $start from its heartbeat"
delivery=$(jq -r --arg id "$start" '.commands[] | select(.command.id == $id) | .delivery' "$out")
[ "$delivery" = done ] || fail "start $start is $delivery, not done: $(cat "$out")"
state=$(agent_is)
[ "$state" = "running true" ] || fail "agent $agent is '$state' once started, not 'running true'"
echo "started $agent: start $start reported done, and the agent is running"

status=$(api POST "/fleet/agents/$agent/stop" '{}')
[ "$status" = 202 ] || fail "stopping $agent: $status $(cat "$out")"
stop=$(jq -er .command.id "$out")
heartbeat
grep -q "^command $stop (stop): done$" "$log" || fail "the host did not take stop $stop from its heartbeat"
state=$(agent_is)
[ "$state" = "stopped false" ] || fail "agent $agent is '$state' once stopped, not 'stopped false'"
echo "stopped $agent: stop $stop reported done, and the agent is no longer active"

proved true
