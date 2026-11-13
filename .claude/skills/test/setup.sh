#!/bin/sh
# Puts what `cargo` needs to check agent-host under TOOLS_DIR, for a machine
# without it: rustup and a stable toolchain with rustfmt and clippy, and zig as
# the C compiler and archiver (ring, which rustls needs, has C). Run it again
# and it skips what is there. Prints the PATH line to use after it, and only
# that on stdout, so `eval "$(setup.sh DIR)"` works: everything else goes to stderr.
set -eu
tools=$(mkdir -p "${1:?usage: setup.sh TOOLS_DIR}" && cd "$1" && pwd)
zig_version=0.13.0
export RUSTUP_HOME="$tools/rustup" CARGO_HOME="$tools/cargo"
mkdir -p "$tools/bin"
if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal -c rustfmt,clippy >&2
fi
if ! command -v cc >/dev/null && [ ! -x "$tools/bin/cc" ]; then
  zig="$tools/zig-linux-x86_64-$zig_version/zig"
  [ -x "$zig" ] || curl -sSfL "https://ziglang.org/download/$zig_version/zig-linux-x86_64-$zig_version.tar.xz" | tar -xJ -C "$tools"
  # cc-rs passes --target=<rust triple>, which zig cannot parse: drop it.
  cat > "$tools/bin/cc" <<CC
#!/bin/sh
for a; do shift; case \$a in --target=*) ;; *) set -- "\$@" "\$a" ;; esac; done
exec "$zig" cc -target x86_64-linux-gnu "\$@"
CC
  printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$zig" > "$tools/bin/ar"
  ln -sf "$zig" "$tools/bin/zig"
  chmod +x "$tools/bin/cc" "$tools/bin/ar"
fi
echo "export RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME PATH=$tools/bin:$CARGO_HOME/bin:\$PATH"
