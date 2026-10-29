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
# The workers' shared context and skills: Claude Code reads the .claude folder
# above every worker's own, so the host never needs to know what is in it.
# WORKER_SKILLS names another repo, or empty for none.
skills_repo=${WORKER_SKILLS-dakotacolorado/worker-skills}
workers_dir=$(sed -n 's/^workers_dir *= *"\(.*\)"/\1/p' "$config")
skills=${workers_dir:-$HOME/workers}/.claude
if [ -z "$skills_repo" ]; then
  :
elif [ -d "$skills/.git" ]; then
  (cd "$skills" && gh repo sync) || echo "install-host: $skills not updated; see git -C $skills status" >&2
elif [ -e "$skills" ]; then
  echo "install-host: $skills is there and not a clone; skills left alone" >&2
else
  gh repo clone "$skills_repo" "$skills" -- --quiet
fi
"$HOME/.local/bin/worker-host" --version
echo "start it:   ~/.local/bin/worker-host 2>> ~/worker-host.log &"
echo "attach:     tmux -L worker-host attach -t worker-<name>"
