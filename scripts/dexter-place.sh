#!/usr/bin/env bash
set -euo pipefail

raw_command="${1:-snap}"
case "$raw_command" in
  snap|move|center)
    command="snap"
    ;;
  start|hold|drag)
    command="start"
    ;;
  stop|release|end)
    command="stop"
    ;;
  *)
    echo "usage: $0 [snap|move|center|start|hold|drag|stop|release|end]" >&2
    exit 2
    ;;
esac

DEXTER_PLACEMENT_COMMAND="$command" /usr/bin/osascript -l JavaScript <<'JXA'
ObjC.import('Foundation');

const env = $.NSProcessInfo.processInfo.environment;
const command = env.objectForKey('DEXTER_PLACEMENT_COMMAND') || $('snap');
const userInfo = $.NSMutableDictionary.dictionary;
userInfo.setObjectForKey(command, $('command'));

$.NSDistributedNotificationCenter.defaultCenter.postNotificationNameObjectUserInfoDeliverImmediately(
  $('com.dexter.placementCommand'),
  $('DexterPlacement'),
  userInfo,
  true
);
JXA
