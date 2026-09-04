#!/usr/bin/env zsh
# Ad-hoc-sign the built binary with the debugger entitlement before running it.
# task_for_pid() needs com.apple.security.cs.debugger; without it the reader
# cannot open a handle to the Cogmind process.
#
# Paths resolve relative to this script so the repo can live anywhere (the
# previous version hardcoded ~/sources/statmind).
set -e
here=${0:A:h}
ident=${STATMIND_CODESIGN_ID:-9E4DDC0A250D30CB8BEB148C8F5EDC283610D680}
codesign --entitlements "$here/entitlements.xml" -fs "$ident" "$1"
exec "$1" "$2"
