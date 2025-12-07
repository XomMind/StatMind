#!/usr/bin/env zsh
codesign --entitlements entitlements.xml -fs 9E4DDC0A250D30CB8BEB148C8F5EDC283610D680 "$1"
exec "$1"