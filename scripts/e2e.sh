#!/bin/sh
# 実機 e2e テストを実行し、管理対象外コンテナ(asb.managed=true を持たない全コンテナ)の
# (ID, 状態, 開始時刻) が実行前後で変わっていないことを確かめる(AC-4c)。
# cargo test が失敗しても比較は行う。どちらかが失敗すれば非ゼロで終了する。
set -u
cd "$(dirname "$0")/.."

snapshot() {
  container ls -a --format json | jq -c '[.[]
    | select((.configuration.labels["asb.managed"] // "") != "true")
    | select((.configuration.labels["asb.e2e-decoy"] // "") != "1")
    | {id: .configuration.id, state: .status.state, started: .status.startedDate}]
    | sort_by(.id)'
}

BEFORE=$(snapshot) || { echo "e2e.sh: cannot list containers" >&2; exit 2; }
cargo test --features e2e --test e2e -- --test-threads=1 "$@"
TEST_RC=$?
AFTER=$(snapshot) || { echo "e2e.sh: cannot list containers" >&2; exit 2; }

if [ "$BEFORE" = "$AFTER" ]; then
  echo "AC-4c: unmanaged containers unchanged: $(printf '%s' "$AFTER" | jq -r 'map(.id) | join(", ")')"
  CMP_RC=0
else
  echo "AC-4c: unmanaged containers CHANGED" >&2
  echo "before: $BEFORE" >&2
  echo "after:  $AFTER" >&2
  CMP_RC=1
fi

LEFT_C=$(container ls -a --format json | jq -r '.[] | .configuration.id' | grep -c '^asb-e2e-')
LEFT_I=$(container image ls --format json | jq -r '.[] | .configuration.name' | grep -c 'asb-archive/asb-e2e-')
echo "leftover asb-e2e- containers: $LEFT_C, asb-archive/asb-e2e- images: $LEFT_I"

[ "$TEST_RC" -eq 0 ] && [ "$CMP_RC" -eq 0 ] && [ "$LEFT_C" -eq 0 ] && [ "$LEFT_I" -eq 0 ]
