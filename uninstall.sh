#!/usr/bin/env bash
# Undo install.sh: stop and remove the systemd --user service and the binary
# in ~/.local/bin. Accounts, tokens and the mail cache stay, so a reinstall --
# or Rustle's built-in bridge, which uses the same setup -- carries on.
#   ./uninstall.sh          service and binary
#   ./uninstall.sh --purge  also unregister from Evolution Data Server and
#                           delete accounts, tokens and the cache
set -euo pipefail

purge=false
for arg in "$@"; do
  case $arg in
    --purge) purge=true ;;
    -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

bin=~/.local/bin/graphmail-bridge
config_dir=${XDG_CONFIG_HOME:-$HOME/.config}/graphmail-bridge
data_dir=${XDG_DATA_HOME:-$HOME/.local/share}/graphmail-bridge
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/graphmail-bridge.service

# The service goes first, so it can't restart the binary being removed.
if [ -f "$unit" ]; then
  systemctl --user disable --now graphmail-bridge.service 2>/dev/null || true
  rm -f "$unit"
  systemctl --user daemon-reload
  echo "removed: graphmail-bridge.service"
fi

if $purge; then
  # eds-setup --remove needs the binary and the config, so it runs before
  # either goes. It takes the EDS sources and the password EDS kept.
  if [ -x "$bin" ] && [ -f "$config_dir/config.toml" ]; then
    sed -n 's/^name *= *"\(.*\)"$/\1/p' "$config_dir/config.toml" | while IFS= read -r account; do
      "$bin" eds-setup --remove "$account" \
        || echo "warning: could not unregister $account from Evolution Data Server" >&2
    done
  fi
  # Tokens kept in the keyring (the default secrets backend).
  if command -v secret-tool >/dev/null; then
    secret-tool clear service dev.graphmail.bridge 2>/dev/null || true
  fi
  rm -rf "$config_dir" "$data_dir"
  echo "removed: accounts, tokens and cache ($config_dir, $data_dir)"
fi

if [ -e "$bin" ]; then
  rm -f "$bin"
  echo "removed: $bin"
fi
other=$(command -v graphmail-bridge || true)
if [ -n "$other" ]; then
  echo "note: another copy is still on PATH at $other (cargo uninstall graphmail-bridge?)"
fi

if command -v rustle >/dev/null || [ -x ~/.local/bin/rustle ]; then
  if $purge; then
    echo "note: Rustle's Microsoft 365 account used this setup and will stop working."
  elif [ -f "$config_dir/config.toml" ]; then
    echo "note: Rustle's built-in bridge takes over the next time Rustle starts."
  fi
fi
