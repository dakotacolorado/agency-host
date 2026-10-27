#!/bin/sh
# Makes this machine a fleet host, or takes it back out. The same script on
# every machine, mac or linux; run it again to take a newer release.
#
# The service url and token come from AGENT_HOST_SERVICE_URL and
# AGENT_HOST_TOKEN, else the config file an earlier install wrote, else a
# prompt. The token is written to that file alone, readable by this user alone.
# Capacity, labels and repositories come from the environment, else that file;
# there is no prompt for them, since a host with none still beats, and is
# offered nothing.
set -eu

usage="usage: install-host.sh [install] [--release TAG | --binary PATH]
       install-host.sh rollback   back to the binary installed before this one
       install-host.sh remove     stop and remove agent-host and its config

  --release TAG  a release of dakotacolorado/agent-host (default: the latest)
  --binary PATH  a binary built here instead of a release

  AGENT_HOST_SERVICE_URL  the fleet api, https only
  AGENT_HOST_TOKEN        its token
  AGENT_HOST_ID           this host's name (default: the hostname)
  AGENT_HOST_CAPACITY     how many agents it will run (unset: none are placed here)
  AGENT_HOST_LABELS       what it can run, comma-separated, such as repo:agent-service
  AGENT_HOST_REPOSITORIES the checkouts agents get worktrees of, as name=/path,..."

repo=dakotacolorado/agent-host
label=com.dakotacolorado.agent-host
bin=$HOME/.local/bin/agent-host
previous=$HOME/.local/share/agent-host/agent-host.previous
config=${XDG_CONFIG_HOME:-$HOME/.config}/agent-host/config.toml
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/agent-host.service
plist=/Library/LaunchDaemons/$label.plist
os=$(uname -s)

say() { echo "install-host: $*"; }
die() {
  echo "install-host: $*" >&2
  exit 1
}

target() {
  case $os in
  Darwin)
    # A shell under Rosetta reports x86_64 on apple silicon.
    if [ "$(sysctl -in hw.optional.arm64)" = 1 ]; then echo aarch64-apple-darwin; else echo x86_64-apple-darwin; fi
    ;;
  Linux)
    case $(uname -m) in
    x86_64) echo x86_64-unknown-linux-musl ;;
    aarch64 | arm64) echo aarch64-unknown-linux-musl ;;
    *) die "there is no agent-host build for $(uname -m)" ;;
    esac
    ;;
  *) die "there is no agent-host build for $os" ;;
  esac
}

from_config() {
  if [ -f "$config" ]; then sed -n "s/^$1 *= *\"\(.*\)\"\$/\1/p" "$config" | head -1; fi
}

# A line as it stands, for the settings that are not strings.
config_line() {
  if [ -f "$config" ]; then grep "^$1 *=" "$config" | head -1; fi
}

ask() {
  [ -t 0 ] || die "no $1: set $2"
  printf '%s: ' "$1" >&2
  if [ "$1" = token ]; then
    stty -echo
    read -r answer || true
    stty echo
    echo >&2
  else
    read -r answer || true
  fi
  echo "$answer"
}

settings() {
  url=${AGENT_HOST_SERVICE_URL:-$(from_config service_url)}
  [ -n "$url" ] || url=$(ask "fleet api url" AGENT_HOST_SERVICE_URL)
  url=${url%/}
  case $url in
  https://?*) ;;
  *) die "the fleet api is reached over https only, not '$url'" ;;
  esac
  token=${AGENT_HOST_TOKEN:-$(from_config token)}
  [ -n "$token" ] || token=$(ask token AGENT_HOST_TOKEN)
  [ -n "$token" ] || die "no token"
  host_id=${AGENT_HOST_ID:-$(from_config host_id)}
  # Written into TOML strings without escaping.
  case $url$token$host_id in *[\"\\]*) die "the url, token and host id cannot hold a quote or a backslash" ;; esac
  capacity=${AGENT_HOST_CAPACITY:-$(config_line capacity | sed 's/^[^=]*= *//')}
  case $capacity in *[!0-9]*) die "the capacity is a whole number of agents, not '$capacity'" ;; esac
  [ -n "$capacity" ] || say "no capacity: set AGENT_HOST_CAPACITY, or no agent will be placed on this host"
  if [ -n "${AGENT_HOST_LABELS:-}" ]; then
    case $AGENT_HOST_LABELS in *[\"\\]*) die "labels cannot hold a quote or a backslash" ;; esac
    labels="labels = [\"$(echo "$AGENT_HOST_LABELS" | sed 's/ *, */", "/g')\"]"
  else
    labels=$(config_line labels)
  fi
  if [ -n "${AGENT_HOST_REPOSITORIES:-}" ]; then
    case $AGENT_HOST_REPOSITORIES in *[\"\\]*) die "repositories cannot hold a quote or a backslash" ;; esac
    pairs=$(echo "$AGENT_HOST_REPOSITORIES" | tr ',' '\n' |
      sed -n 's/^ *\([A-Za-z0-9][A-Za-z0-9._-]*\) *= *\(\/.*[^ ]\) *$/"\1" = "\2"/p' | paste -sd, -)
    [ "$(echo "$pairs" | tr ',' '\n' | grep -c .)" = "$(echo "$AGENT_HOST_REPOSITORIES" | tr ',' '\n' | grep -c .)" ] ||
      die "repositories are name=/absolute/path, comma-separated, not '$AGENT_HOST_REPOSITORIES'"
    repositories="repositories = { $pairs }"
  else
    repositories=$(config_line repositories)
  fi
}

tailscale_cli() {
  command -v tailscale && return
  app=/Applications/Tailscale.app/Contents/MacOS/Tailscale
  [ -x "$app" ] && echo "$app"
}

reachable() { nc -z -w 3 "$1" "$2" > /dev/null 2>&1; }

# Attaching to a worker is ssh over the tailnet, then tmux. The agent itself
# only ever calls out, so nothing has to reach this machine but that.
tailnet() {
  ts=$(tailscale_cli) || die "tailscale is not installed: get it from https://tailscale.com/download, sign in, and run this again"
  ip=$("$ts" ip -4 2> /dev/null | head -1) || true
  [ -n "$ip" ] || die "this machine is not on the tailnet: sign in to tailscale (tailscale up) and run this again"
  if "$ts" funnel status 2> /dev/null | grep -qi 'funnel on'; then
    die "tailscale funnel publishes this machine beyond the tailnet: turn it off (tailscale funnel reset) and run this again"
  fi
  if ! reachable "$ip" 22; then
    case $os in
    # Tailscale SSH answers on the tailnet address and nowhere else.
    Linux) sudo "$ts" set --ssh ;;
    # The mac app's tailscale cannot serve ssh, so the mac's own sshd does.
    Darwin)
      say "turning on Remote Login, so the tailnet can attach"
      sudo launchctl enable system/com.openssh.sshd
      sudo launchctl bootstrap system /System/Library/LaunchDaemons/ssh.plist 2> /dev/null || true
      ;;
    esac
    sleep 1
    reachable "$ip" 22 || die "nothing answers ssh at $ip: turn on Remote Login (mac) or Tailscale SSH (linux) and run this again"
  fi
}

need_tmux() {
  command -v tmux > /dev/null && return
  if command -v brew > /dev/null; then
    brew install tmux
  elif command -v apt-get > /dev/null; then
    sudo apt-get update && sudo apt-get install -y tmux
  elif command -v dnf > /dev/null; then
    sudo dnf install -y tmux
  elif command -v pacman > /dev/null; then
    sudo pacman -S --noconfirm tmux
  else
    die "tmux is missing, and there is no package manager here to install it with"
  fi
}

sha256_check() {
  if command -v sha256sum > /dev/null; then sha256sum -c; else shasum -a 256 -c; fi
}

fetch() {
  if [ -n "$from" ]; then
    cp "$from" "$stage/agent-host"
  else
    command -v gh > /dev/null || die "gh downloads the release: install it and gh auth login, or pass --binary"
    gh release download ${tag:+"$tag"} --repo "$repo" --pattern "agent-host-$t" --pattern SHA256SUMS --dir "$stage"
    (cd "$stage" && grep " agent-host-$t\$" SHA256SUMS | sha256_check) > /dev/null ||
      die "agent-host-$t does not match its release's SHA256SUMS"
    mv "$stage/agent-host-$t" "$stage/agent-host"
  fi
  chmod +x "$stage/agent-host"
}

write_config() {
  rm -f "$1"
  (
    umask 077
    printf 'service_url = "%s"\ntoken = "%s"\n' "$url" "$token"
    if [ -n "$host_id" ]; then printf 'host_id = "%s"\n' "$host_id"; fi
    if [ -n "$capacity" ]; then printf 'capacity = %s\n' "$capacity"; fi
    if [ -n "$labels" ]; then printf '%s\n' "$labels"; fi
    if [ -n "$repositories" ]; then printf '%s\n' "$repositories"; fi
  ) > "$1"
}

# A mac LaunchAgent stops at logout and waits for a login after a reboot; a
# daemon run as this user does neither.
service_darwin() {
  log=$HOME/Library/Logs/agent-host.log
  mkdir -p "$(dirname "$log")"
  touch "$log"
  cat > "$stage/plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array><string>$bin</string><string>--config</string><string>$config</string></array>
  <key>UserName</key><string>$(id -un)</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>$HOME</string>
    <key>PATH</key><string>$path</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
EOF
  sudo install -m 644 -o root -g wheel "$stage/plist" "$plist"
  sudo launchctl bootout "system/$label" 2> /dev/null || true
  # bootout returns before the old job is gone, and bootstrap fails until it is.
  for _ in 1 2 3 4 5; do
    sudo launchctl bootstrap system "$plist" 2> /dev/null && return
    sleep 1
  done
  sudo launchctl bootstrap system "$plist"
}

service_linux() {
  mkdir -p "$(dirname "$unit")"
  cat > "$unit" << EOF
[Unit]
Description=agent-host: this machine's place in the fleet

[Service]
ExecStart=$bin --config $config
Environment=PATH=$path
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
EOF
  # Without linger, this user's services stop at logout and wait for a login
  # after a reboot.
  loginctl enable-linger "$(id -un)" 2> /dev/null || sudo loginctl enable-linger "$(id -un)"
  systemctl --user daemon-reload
  systemctl --user enable agent-host
  systemctl --user restart agent-host
}

restart() {
  case $os in
  Darwin) sudo launchctl kickstart -k "system/$label" ;;
  Linux) systemctl --user restart agent-host ;;
  esac
}

install_host() {
  t=$(target)
  settings
  tailnet
  need_tmux
  claude=$(command -v claude) || die "claude is not on the PATH: install Claude Code, sign in, and run this again"
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  fetch
  # One beat from the new binary before it replaces anything: a host that
  # cannot reach the fleet api is not ready, and the one running stays put.
  write_config "$stage/config.toml"
  "$stage/agent-host" --config "$stage/config.toml" --beats 1 ||
    die "the new binary could not beat to $url; nothing was replaced"

  mkdir -p "$(dirname "$bin")" "$(dirname "$previous")" "$(dirname "$config")"
  mv "$stage/config.toml" "$config"
  if [ -f "$bin" ] && ! cmp -s "$bin" "$stage/agent-host"; then cp -p "$bin" "$previous"; fi
  cp "$stage/agent-host" "$bin.new"
  mv "$bin.new" "$bin"

  # The service gets none of the login shell's PATH, and Claude Code installs
  # itself to ~/.local/bin.
  path="$(dirname "$claude"):$(dirname "$(command -v tmux)"):/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  "service_$(echo "$os" | tr '[:upper:]' '[:lower:]')"
  say "$("$bin" --version) is running as a service, beating to $url"
  say "attach from the tailnet: ssh $(id -un)@$ip, then tmux attach"
}

rollback() {
  [ -f "$previous" ] || die "nothing to roll back to: one binary at most has been installed here"
  mv "$bin" "$bin.swap"
  mv "$previous" "$bin"
  mv "$bin.swap" "$previous"
  restart
  say "rolled back to $("$bin" --version); roll back again to return"
}

remove() {
  case $os in
  Darwin)
    sudo launchctl bootout "system/$label" 2> /dev/null || true
    sudo rm -f "$plist"
    ;;
  Linux)
    systemctl --user disable --now agent-host 2> /dev/null || true
    rm -f "$unit"
    systemctl --user daemon-reload
    ;;
  esac
  rm -f "$bin" "$previous" "$config"
  # Interactive sessions use them too.
  say "agent-host and its config are gone; tmux, tailscale, ssh and linger are left as they were"
}

command=install
from=
tag=
case ${1:-} in install | rollback | remove)
  command=$1
  shift
  ;;
esac
while [ $# -gt 0 ]; do
  case $1 in
  --binary)
    from=${2:?--binary needs a path}
    shift 2
    ;;
  --release)
    tag=${2:?--release needs a tag}
    shift 2
    ;;
  -h | --help)
    echo "$usage"
    exit 0
    ;;
  *) die "unknown argument $1

$usage" ;;
  esac
done
[ "$command" = install ] || [ -z "$from$tag" ] || die "--binary and --release are for install"

case $command in
install) install_host ;;
rollback) rollback ;;
remove) remove ;;
esac
