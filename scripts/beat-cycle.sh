#!/bin/sh
# Proves the built binary and the service agree on a whole beat cycle: the host
# registers by beating, takes a command queued for it, and reports it back.
#
#   AGENT_HOST_SERVICE_URL=… AGENT_HOST_TOKEN=… AGENT_HOST_ID=… beat-cycle.sh BINARY
#
# Writes proved=true|false to $GITHUB_OUTPUT. A service that does not serve the
# beat yet is a dependency still to come, not a failure: it ends green with
# proved=false, and nothing is released on it. Anything else that goes wrong
# fails.
set -eu

bin=$1
url=${AGENT_HOST_SERVICE_URL%/}
host=$AGENT_HOST_ID
auth="Authorization: Bearer $AGENT_HOST_TOKEN"
log=$(mktemp)

proved() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "proved=$1" >> "$GITHUB_OUTPUT"; fi
}
not_yet() {
  echo "::warning::$1; the beat cycle is unproved and nothing will be released"
  proved false
  exit 0
}

if ! "$bin" --beats 1 2> "$log"; then
  cat "$log"
  grep -q '^beat failed: 404 ' "$log" && not_yet "the service does not serve POST /fleet/hosts/{id}/beat yet"
  exit 1
fi
echo "registered $host"

status=$(curl -s -o "$log" -w '%{http_code}' -X POST -H "$auth" -H 'Content-Type: application/json' \
  -d '{"kind":"ping"}' "$url/fleet/hosts/$host/commands")
[ "$status" = 404 ] && not_yet "the service does not serve POST /fleet/hosts/{id}/commands yet"
[ "$status" = 201 ] || { echo "queueing a command: $status $(cat "$log")"; exit 1; }
command=$(jq -er .id "$log")
echo "queued ping $command"

# The first beat takes the command, the second carries its report.
"$bin" --beats 2

curl -sf -H "$auth" "$url/fleet/hosts/$host" > "$log"
report=$(jq -c --arg id "$command" '.commands[] | select(.id == $id) | .report' "$log")
echo "reported: $report"
[ "$(echo "$report" | jq -r .ok)" = true ] || { cat "$log"; exit 1; }
proved true
