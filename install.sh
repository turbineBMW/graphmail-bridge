#!/usr/bin/env bash
# User-local install: binary and systemd --user service. No root needed.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --locked
install -Dm755 target/release/graphmail-bridge ~/.local/bin/graphmail-bridge

case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "warning: ~/.local/bin is not on PATH" ;;
esac

# install-service bakes its own current_exe() into ExecStart, so it has to run
# from the installed copy rather than target/release. It also needs a config,
# which only `setup` can write.
config=${XDG_CONFIG_HOME:-$HOME/.config}/graphmail-bridge/config.toml
if [ -f "$config" ]; then
  ~/.local/bin/graphmail-bridge install-service
  # enable --now leaves an already-running unit on the old binary.
  systemctl --user restart graphmail-bridge.service
  echo "installed: ~/.local/bin/graphmail-bridge (service restarted)"
else
  echo "installed: ~/.local/bin/graphmail-bridge"
  echo "next: graphmail-bridge setup && graphmail-bridge doctor && graphmail-bridge install-service"
fi
