#!/bin/sh
# Fake `brew` for tests (`FUIDE_BREW_BIN` points here). Read-only queries answer from the
# fixture JSON next to this script; mutating commands echo what they were asked to do.
# `boom` as a package name fails like a real brew error. Every call is appended to
# `$FUIDE_FAKE_BREW_LOG` when set, so tests can assert what was run.
here=$(cd "$(dirname "$0")" && pwd)
if [ -n "$FUIDE_FAKE_BREW_LOG" ]; then
  printf '%s\n' "$*" >> "$FUIDE_FAKE_BREW_LOG"
fi
case "$1" in
  --version) echo "Homebrew 4.9.9" ;;
  --prefix) echo "/tmp/fuide-fake-brew-prefix" ;;
  --repository) echo "/tmp/fuide-fake-brew-prefix" ;;
  info)
    # `info --json=v2 --installed` = everything; `info --json=v2 --formula|--cask <names>`
    # (the search detail pass) = only that kind, so hits are not duplicated
    case "$*" in
      *--cask*) cat "$here/info-casks.json" ;;
      *--formula*) cat "$here/info-formulae.json" ;;
      *) cat "$here/info-installed.json" ;;
    esac
    ;;
  search)
    # `brew search --formula|--cask <query>`: a couple of hits for anything
    case "$2" in
      --cask) echo "iterm2" ;;
      *) echo "ripgrep"; echo "ripgrep-all" ;;
    esac
    ;;
  update|upgrade|install|uninstall|pin|unpin)
    echo "==> $*"
    for a in "$@"; do
      if [ "$a" = "boom" ]; then
        echo "Error: boom" >&2
        exit 1
      fi
    done
    echo "done $1"
    ;;
  *) echo "fake brew: unknown command $*" >&2; exit 2 ;;
esac
exit 0
