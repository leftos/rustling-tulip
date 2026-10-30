#!/usr/bin/env bash
# RT-581 mock: shoot the given views for each given theme.
# Usage: bash docs/design/ui-language/shoot.sh "<themes>" "<views>"
set -u
cd "$(dirname "$0")/../../.."
themes=${1:-"current a b c"}
views=${2:-"menu main connections settings"}
status=0
for theme in $themes; do
  for view in $views; do
    out=".tmp/shots/$theme-$view.png"
    RT_MOCK_THEME=$theme pwsh -NoProfile -File ./rt.ps1 native-shot "$view" "$out" > ".tmp/shot-$theme-$view.log" 2>&1
    rc=$?
    echo "$theme $view rc=$rc $(tail -n 2 ".tmp/shot-$theme-$view.log" | head -n 1)"
    [ $rc -ne 0 ] && status=1
  done
done
exit $status
