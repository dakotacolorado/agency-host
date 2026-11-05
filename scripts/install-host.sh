#!/bin/sh
# Puts the latest worker-host release on this machine and, the first time, a
# config file for it. Run it again for a newer release; the config is kept.
set -eu
usage="usage: install-host.sh SERVICE_URL TOKEN   (both needed only the first time)"
config=${XDG_CONFIG_HOME:-$HOME/.config}/worker-host/config.toml
case $(uname -s)-$(uname -m) in
Darwin-*) [ "$(sysctl -in hw.optional.arm64)" = 1 ] && t=aarch64-apple-darwin || t=x86_64-apple-darwin ;;
Linux-x86_64) t=x86_64-unknown-linux-musl ;;
Linux-aarch64 | Linux-arm64) t=aarch64-unknown-linux-musl ;;
*) echo "install-host: no build for $(uname -sm)" >&2 && exit 1 ;;
esac
mkdir -p "$HOME/.local/bin" "$(dirname "$config")"
gh release download --repo dakotacolorado/worker-host --pattern "worker-host-$t" --output "$HOME/.local/bin/.worker-host.new" --clobber
chmod +x "$HOME/.local/bin/.worker-host.new"
# A rename, so a host already running keeps the binary it started with.
mv "$HOME/.local/bin/.worker-host.new" "$HOME/.local/bin/worker-host"
if [ ! -f "$config" ]; then
  [ $# -eq 2 ] || { echo "$usage" >&2 && exit 2; }
  (umask 077 && printf 'service_url = "%s"\ntoken = "%s"\nhost_id = "%s"\ncapacity = 1\nlabels = ["%s"]\nworkers_dir = "%s"\n' \
    "$1" "$2" "$(hostname -s)" "$(uname -s | tr 'A-Z' 'a-z' | sed s/darwin/macos/)" "$HOME/workers" > "$config")
  echo "wrote $config: edit capacity and labels to suit this machine"
fi
# Sessions end with the host, so stopping the unit may end its whole cgroup,
# and what each session's tools started with it. An earlier release had the
# unit stop only the host, with this drop-in, which is taken away.
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
if [ -f "$units/worker-host.service.d/keep-sessions.conf" ]; then
  rm "$units/worker-host.service.d/keep-sessions.conf"
  rmdir "$units/worker-host.service.d" 2>/dev/null || true
  command -v systemctl >/dev/null && systemctl --user daemon-reload
fi
# Workers' context now comes with each session's start, so the skills clone
# and its sync, from before, are taken away: the marked cron entry, and a
# clone at <workers_dir>/.claude left clean.
if command -v crontab >/dev/null && crontab -l 2>/dev/null | grep -qF "# worker-host: sync "; then
  crontab -l | grep -vF "# worker-host: sync " | crontab -
fi
workers_dir=$(sed -n 's/^workers_dir *= *"\(.*\)"/\1/p' "$config")
skills=${workers_dir:-$HOME/workers}/.claude
if [ -d "$skills/.git" ]; then
  if [ -z "$(git -C "$skills" status --porcelain)" ]; then
    rm -rf "$skills"
  else
    echo "install-host: $skills has local changes; left in place, remove it once they are saved" >&2
  fi
fi
"$HOME/.local/bin/worker-host" --version
# On a mac, launchd runs the host: from login, again whenever it exits, and
# never with a worker's environment, so it always finds the user's own Claude
# login. launchd gives a job a bare PATH, so the agent is given one that has
# claude and gh.
if [ "$(uname -s)" = Darwin ]; then
  label=com.github.dakotacolorado.worker-host
  agent=$HOME/Library/LaunchAgents/$label.plist
  domain=gui/$(id -u)
  mkdir -p "$(dirname "$agent")"
  cat > "$agent" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$HOME/.local/bin/worker-host</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/dev/null</string>
  <key>StandardErrorPath</key><string>$HOME/worker-host.log</string>
</dict>
</plist>
EOF
  if launchctl print "$domain/$label" >/dev/null 2>&1; then
    # launchd stops and starts it, so this finishes even when run from a
    # session of the host it restarts.
    launchctl kickstart -k "$domain/$label"
    echo "restarted $label on this release: each live session is resumed"
  else
    # A host started by hand gives way to the agent. Detached, since this may
    # run in a session of that host, which ends with it.
    nohup sh -c 'pkill -x worker-host; while pgrep -x worker-host >/dev/null; do sleep 1; done; launchctl bootstrap "$1" "$2"' \
      sh "$domain" "$agent" >/dev/null 2>&1 </dev/null &
    echo "started $label from $agent, in place of any host started by hand: each live session is resumed"
  fi
  exit 0
fi
echo "start it:   ~/.local/bin/worker-host 2>> ~/worker-host.log &"
echo "restart it to run this release: each live session is resumed, losing only the turn in flight"
