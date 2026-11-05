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
# Under systemd, stopping the unit kills its whole cgroup. Sessions end with
# the host all the same, each told to as it goes, but a release from before
# ran them in tmux, which must outlive this restart for the new host to take
# them over: so stopping the unit stops only the host.
if command -v systemctl >/dev/null && systemctl --user cat worker-host.service >/dev/null 2>&1; then
  units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
  mkdir -p "$units/worker-host.service.d"
  printf '[Service]\nKillMode=process\n' > "$units/worker-host.service.d/keep-sessions.conf"
  systemctl --user daemon-reload
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
echo "start it:   ~/.local/bin/worker-host 2>> ~/worker-host.log &"
echo "restart it to run this release: each live session is resumed, losing only the turn in flight"
