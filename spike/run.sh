#!/bin/bash
# Phase 0: Apple Container の前提 U-1〜U-10 を実測する。
#
# 安全規則:
#   - 検証用コンテナは名前接頭辞 asb-spike- とラベル asb.spike=1 を持つ。
#   - 削除はラベル asb.spike=1 を持つコンテナ、asb-spike/ 配下のイメージ、
#     ラベル asb.spike=1 を持つネットワーク asb-spike-internal に限る。
#   - container prune / stop --all / system stop / build は使わない。
#   - 実行前後で asb.managed=true を持たない全コンテナの状態を比較する。
#
# 使い方: bash spike/run.sh   (結果は spike/out/ に出る)

set -u
cd "$(dirname "$0")"
OUT=out
mkdir -p "$OUT"
IMAGE=alpine:3.22
NET=asb-spike-internal
EXT_HOST=1.1.1.1
EXT_PORT=443
RESULTS="$OUT/results.md"
: > "$RESULTS"

log() { printf '%s\n' "$*" | tee -a "$RESULTS"; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time*1000'; }

unmanaged_snapshot() {
  container ls -a --format json | jq -c '[.[]
    | select((.configuration.labels["asb.managed"] // "") != "true")
    | select((.configuration.labels["asb.spike"] // "") != "1")
    | {id: .configuration.id, state: .status.state, started: .status.startedDate}]
    | sort_by(.id)'
}

state_of() {
  container ls -a --format json | jq -r --arg n "$1" '.[] | select(.configuration.id==$n) | .status.state'
}

is_spike() {
  [ "$(container ls -a --format json | jq -r --arg n "$1" '.[] | select(.configuration.id==$n) | .configuration.labels["asb.spike"] // ""')" = "1" ]
}

# ラベル asb.spike=1 と名前接頭辞の両方を満たすものだけ削除する。
spike_rm() {
  case "$1" in asb-spike-*) ;; *) echo "refuse rm: $1" >&2; return 1 ;; esac
  if is_spike "$1"; then
    container stop "$1" >/dev/null 2>&1
    container rm "$1" >/dev/null 2>&1
  fi
}

spike_image_rm() {
  case "$1" in asb-spike/*) container image rm "$1" >/dev/null 2>&1 ;; *) echo "refuse image rm: $1" >&2 ;; esac
}

spike_create() { # name [extra args...]
  local n="$1"; shift
  container create --name "$n" --label asb.spike=1 --init --entrypoint sleep "$@" "$IMAGE" infinity >/dev/null
}

spike_create_from() { # name image [extra args...]
  local n="$1" img="$2"; shift 2
  container create --name "$n" --label asb.spike=1 --init --entrypoint sleep "$@" "$img" infinity >/dev/null
}

cleanup() {
  for c in $(container ls -a --format json | jq -r '.[] | select(.configuration.labels["asb.spike"]=="1") | .configuration.id'); do
    spike_rm "$c"
  done
  for i in $(container image ls --format json 2>/dev/null | jq -r '.[] | .configuration.name // empty' | sed 's#^docker.io/##' | grep '^asb-spike/'); do
    spike_image_rm "$i"
  done
  if container network ls --format json 2>/dev/null | jq -e --arg n "$NET" '.[] | select(.id==$n and (.configuration.labels["asb.spike"] // "")=="1")' >/dev/null 2>&1; then
    container network rm "$NET" >/dev/null 2>&1
  fi
}

vm_pids() {
  ps -axo pid=,command= | grep 'com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine' | grep -v grep | awk '{print $1}' | sort -n
}

stats() { # 数値列(標準入力)から n, median, max, min を出す
  sort -n | awk '{a[NR]=$1} END {if(NR==0){print "n=0"; exit}; m=(NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2; printf "n=%d median=%s max=%s min=%s\n", NR, m, a[NR], a[1]}'
}

trap cleanup EXIT

log "# Phase 0 raw results"
log ""
log "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
log "- container: $(container --version)"
log "- macOS: $(sw_vers -productVersion) $(uname -m)"
log "- image: $IMAGE"
log ""

unmanaged_snapshot > "$OUT/unmanaged-before.json"
container image pull "$IMAGE" >/dev/null 2>&1

########################################
log "## U-1 stop/start でファイルが残るか"
spike_rm asb-spike-u1
spike_create asb-spike-u1
container start asb-spike-u1 >/dev/null
container exec asb-spike-u1 sh -c 'echo u1-$$-marker > /root/m && sync'
EXPECT=$(container exec asb-spike-u1 cat /root/m)
container stop asb-spike-u1 >/dev/null
S1=$(state_of asb-spike-u1)
container start asb-spike-u1 >/dev/null
GOT=$(container exec asb-spike-u1 cat /root/m 2>&1)
sleep 2
S2=$(state_of asb-spike-u1)
log "- 停止後の状態: $S1 / 再開 2 秒後の状態: $S2"
log "- 書き込み: $EXPECT / 再開後の読み出し: $GOT"
if [ "$EXPECT" = "$GOT" ] && [ "$S1" = "stopped" ] && [ "$S2" = "running" ]; then log "- **U-1: PASS**"; else log "- **U-1: FAIL**"; fi
spike_rm asb-spike-u1
log ""

########################################
log "## U-2 stop でホストのメモリが解放されるか"
U2_OK=0
for i in 1 2 3; do
  n=asb-spike-u2-$i
  spike_rm "$n"
  # tmpfs の mount に CAP_SYS_ADMIN が要る(既定では mount: permission denied)
  spike_create "$n" --memory 1G --cap-add CAP_SYS_ADMIN
  before_ls=$(unmanaged_snapshot)
  before=$(vm_pids)
  container start "$n" >/dev/null
  after=$(vm_pids)
  newpid=$(comm -13 <(echo "$before") <(echo "$after"))
  count=$(echo "$newpid" | grep -c . )
  wrote=$(container exec "$n" sh -c 'mkdir -p /mnt/t && mount -t tmpfs -o size=600m tmpfs /mnt/t && dd if=/dev/urandom of=/mnt/t/f bs=1M count=450 2>/dev/null && sync && du -m /mnt/t/f | cut -f1' 2>&1)
  log "  - 回 $i: tmpfs 書き込み量 ${wrote} MB"
  sleep 1
  fp=$(footprint -p "$newpid" -f bytes 2>/dev/null | awk '/phys_footprint:/ {print $2; exit}')
  vmfree_before=$(vm_stat | awk '/Pages free/ {gsub("\\.","",$3); print $3}')
  container stop "$n" >/dev/null
  sleep 2
  vmfree_after=$(vm_stat | awk '/Pages free/ {gsub("\\.","",$3); print $3}')
  alive=$(ps -p "$newpid" -o pid= 2>/dev/null | tr -d ' ')
  after_ls=$(unmanaged_snapshot)
  fp_mb=$(( ${fp:-0} / 1048576 ))
  others_same=yes; [ "$before_ls" = "$after_ls" ] || others_same=no
  log "- 回 $i: VM pid=$newpid(新規 VM 数 $count)、停止前 phys_footprint=${fp_mb} MB、停止後プロセス存在=${alive:-なし}、他コンテナ不変=$others_same、vm_stat free pages ${vmfree_before} → ${vmfree_after}(参考)"
  if [ "$count" = "1" ] && [ "$fp_mb" -ge 400 ] && [ -z "$alive" ] && [ "$others_same" = yes ]; then U2_OK=$((U2_OK+1)); fi
  spike_rm "$n"
done
if [ "$U2_OK" = 3 ]; then log "- **U-2: PASS** (3/3)"; else log "- **U-2: FAIL** ($U2_OK/3)"; fi
log ""

########################################
log "## U-3 / U-4 commit と復元"
spike_rm asb-spike-u3
spike_create asb-spike-u3
container start asb-spike-u3 >/dev/null
container exec asb-spike-u3 sh -c 'echo u3data > /root/data && chmod 640 /root/data && rm -f /etc/motd && mkdir -p /work && echo w > /work/f && sync'
container commit asb-spike-u3 asb-spike/u3-running:g1 >/dev/null 2>&1 && C_RUN=ok || C_RUN=fail
container stop asb-spike-u3 >/dev/null
container commit asb-spike-u3 asb-spike/u3-stopped:g1 >/dev/null 2>&1 && C_STOP=ok || C_STOP=fail
# 同じ参照名への再 commit
container commit asb-spike-u3 asb-spike/u3-stopped:g1 >"$OUT/u3-recommit.txt" 2>&1 && C_RE=ok || C_RE="fail: $(head -c 200 "$OUT/u3-recommit.txt")"
log "- 稼働中 commit: $C_RUN / 停止中 commit: $C_STOP / 同名参照への再 commit: $C_RE"
if [ "$C_RUN" = ok ] || [ "$C_STOP" = ok ]; then log "- **U-3: PASS**"; else log "- **U-3: FAIL**"; fi
container image inspect "$IMAGE" > "$OUT/u4-base-inspect.json" 2>&1
container image inspect asb-spike/u3-stopped:g1 > "$OUT/u4-commit-inspect.json" 2>&1

U4_OK=0
src=asb-spike-u3
ref=asb-spike/u3-stopped:g1
for g in 1 2 3; do
  n=asb-spike-u4-g$g
  spike_rm "$n"
  spike_create_from "$n" "$ref"
  container start "$n" >/dev/null
  data=$(container exec "$n" cat /root/data 2>&1)
  perm=$(container exec "$n" stat -c %a /root/data 2>&1)
  motd=$(container exec "$n" sh -c 'test -e /etc/motd && echo exists || echo absent')
  work=$(container exec "$n" cat /work/f 2>&1)
  size=$(container image ls --format json | jq -r --arg r "docker.io/$ref" '.[] | select(.configuration.name==$r) | .configuration.descriptor.size' 2>/dev/null | head -1)
  log "- 世代 $g ($ref): data=$data perm=$perm /etc/motd=$motd /work/f=$work index-size=$size"
  if [ "$data" = u3data ] && [ "$perm" = 640 ] && [ "$motd" = absent ] && [ "$work" = w ]; then U4_OK=$((U4_OK+1)); fi
  # 次世代: 書き込み → 停止 → commit → 元コンテナ削除
  container exec "$n" sh -c "echo gen$g >> /root/gens && sync"
  container stop "$n" >/dev/null
  next=asb-spike/u4:g$((g+1))
  container commit "$n" "$next" >/dev/null 2>&1
  spike_rm "$src"
  src=$n
  ref=$next
done
gens=$(spike_create_from asb-spike-u4-final "$ref" && container start asb-spike-u4-final >/dev/null && container exec asb-spike-u4-final cat /root/gens | tr '\n' ' ')
log "- 3 巡後の /root/gens: $gens"
container system df > "$OUT/u4-df.txt" 2>&1
if [ "$U4_OK" = 3 ]; then log "- **U-4: PASS** (3/3)"; else log "- **U-4: FAIL** ($U4_OK/3)"; fi
log "- 元イメージと commit イメージの inspect は out/u4-base-inspect.json と out/u4-commit-inspect.json"
spike_rm asb-spike-u4-final
spike_rm "$src"
log ""

########################################
log "## U-5 internal ネットワークの外向き遮断"
HOST_OK=no
nc -z -G 5 "$EXT_HOST" "$EXT_PORT" >/dev/null 2>&1 && HOST_OK=yes
log "- ホストから $EXT_HOST:$EXT_PORT: $HOST_OK"
if [ "$HOST_OK" != yes ]; then
  log "- **U-5: 測定不能**(ホストから接続できない)"
else
  container network create --internal --label asb.spike=1 "$NET" >/dev/null 2>&1
  container network inspect "$NET" > "$OUT/u5-network.json" 2>&1
  spike_rm asb-spike-u5-int; spike_rm asb-spike-u5-def
  spike_create asb-spike-u5-int --network "$NET"
  spike_create asb-spike-u5-def
  container start asb-spike-u5-int >/dev/null
  container start asb-spike-u5-def >/dev/null
  probe() { container exec "$1" sh -c "nc -w 5 $EXT_HOST $EXT_PORT </dev/null >/dev/null 2>&1; echo \$?"; }
  INT_RC=$(probe asb-spike-u5-int)
  DEF_RC=$(probe asb-spike-u5-def)
  GW_INT=$(container ls --format json | jq -r '.[] | select(.configuration.id=="asb-spike-u5-int") | .status.networks[0].ipv4Gateway')
  GW_RC=$(container exec asb-spike-u5-int sh -c "ping -c1 -W2 $GW_INT >/dev/null 2>&1; echo \$?")
  DNS_INT=$(container exec asb-spike-u5-int sh -c 'nslookup example.com >/dev/null 2>&1; echo $?')
  DNS_DEF=$(container exec asb-spike-u5-def sh -c 'nslookup example.com >/dev/null 2>&1; echo $?')
  log "- 接続コマンド: nc -w 5 $EXT_HOST $EXT_PORT </dev/null(busybox)"
  log "- internal からの接続終了コード: $INT_RC / 既定からの接続終了コード: $DEF_RC"
  log "- internal のゲートウェイ $GW_INT への ping 終了コード: $GW_RC"
  log "- 名前解決 internal: $DNS_INT / 既定: $DNS_DEF(0 が成功)"
  if [ "$INT_RC" != 0 ] && [ "$DEF_RC" = 0 ]; then log "- **U-5: PASS**"; else log "- **U-5: FAIL**"; fi
  spike_rm asb-spike-u5-int; spike_rm asb-spike-u5-def
fi
log ""

########################################
log "## U-6 / U-10 所要時間(11 回、初回を除く 10 回で集計、単位 ms)"
spike_rm asb-spike-u6
spike_create asb-spike-u6
: > "$OUT/u6-wake.txt"; : > "$OUT/u6-stop.txt"; : > "$OUT/u6-archived.txt"
for i in $(seq 1 11); do
  t0=$(now_ms)
  container start asb-spike-u6 >/dev/null
  until container exec asb-spike-u6 true >/dev/null 2>&1; do :; done
  t1=$(now_ms)
  container stop asb-spike-u6 >/dev/null
  t2=$(now_ms)
  if [ "$i" -gt 1 ]; then echo $((t1-t0)) >> "$OUT/u6-wake.txt"; echo $((t2-t1)) >> "$OUT/u6-stop.txt"; fi
done
container commit asb-spike-u6 asb-spike/u6:g1 >/dev/null 2>&1
for i in $(seq 1 11); do
  n=asb-spike-u6-a
  spike_rm "$n"
  t0=$(now_ms)
  spike_create_from "$n" asb-spike/u6:g1
  container start "$n" >/dev/null
  until container exec "$n" true >/dev/null 2>&1; do :; done
  t1=$(now_ms)
  if [ "$i" -gt 1 ]; then echo $((t1-t0)) >> "$OUT/u6-archived.txt"; fi
done
spike_rm asb-spike-u6-a
log "- 起床(stopped → start → 最初の exec 成功): $(stats < "$OUT/u6-wake.txt")"
log "- 停止(U-10, stop の所要時間): $(stats < "$OUT/u6-stop.txt")"
log "- archived からの復元(create + start + 最初の exec 成功): $(stats < "$OUT/u6-archived.txt")"
spike_rm asb-spike-u6
log ""

########################################
log "## U-7 並行操作"
spike_rm asb-spike-u7a; spike_rm asb-spike-u7b
spike_create asb-spike-u7a; spike_create asb-spike-u7b
container start asb-spike-u7a >/dev/null
( container exec asb-spike-u7a sh -c 'sleep 2; echo A' > "$OUT/u7-a.txt" 2>&1; echo $? > "$OUT/u7-a.rc" ) &
( container exec asb-spike-u7a sh -c 'sleep 2; echo B' > "$OUT/u7-b.txt" 2>&1; echo $? > "$OUT/u7-b.rc" ) &
wait
EA="$(cat "$OUT/u7-a.rc"):$(cat "$OUT/u7-a.txt")"; EB="$(cat "$OUT/u7-b.rc"):$(cat "$OUT/u7-b.txt")"
log "- 同一コンテナへの並行 exec: A=$EA B=$EB"
container stop asb-spike-u7a >/dev/null
( container start asb-spike-u7a >/dev/null 2>&1; echo $? > "$OUT/u7-sa.rc" ) &
( container start asb-spike-u7b >/dev/null 2>&1; echo $? > "$OUT/u7-sb.rc" ) &
wait
SA=$(cat "$OUT/u7-sa.rc"); SB=$(cat "$OUT/u7-sb.rc")
sleep 1
log "- 別コンテナへの並行 start: rc a=$SA b=$SB / 状態 a=$(state_of asb-spike-u7a) b=$(state_of asb-spike-u7b)"
# 参考: exec 実行中の stop
( container exec asb-spike-u7a sleep 20 >/dev/null 2>&1; echo $? > "$OUT/u7-stopdur.rc" ) &
sleep 2
container stop asb-spike-u7a >/dev/null
wait
log "- 参考: exec 実行中に stop した時の exec の終了コード: $(cat "$OUT/u7-stopdur.rc")"
if [ "$EA" = "0:A" ] && [ "$EB" = "0:B" ] && [ "$SA" = 0 ] && [ "$SB" = 0 ]; then log "- **U-7: PASS**"; else log "- **U-7: FAIL**"; fi
spike_rm asb-spike-u7a; spike_rm asb-spike-u7b
log ""

########################################
log "## U-8 ラベルの取得"
spike_rm asb-spike-u8
container create --name asb-spike-u8 --label asb.spike=1 --label asb.managed-probe=yes --init --entrypoint sleep "$IMAGE" infinity >/dev/null
container start asb-spike-u8 >/dev/null
L_RUN=$(container ls -a --format json | jq -r '.[] | select(.configuration.id=="asb-spike-u8") | .configuration.labels["asb.managed-probe"] // "none"')
container stop asb-spike-u8 >/dev/null
L_STOP=$(container ls -a --format json | jq -r '.[] | select(.configuration.id=="asb-spike-u8") | .configuration.labels["asb.managed-probe"] // "none"')
log "- 稼働中: $L_RUN / 停止中: $L_STOP"
if [ "$L_RUN" = yes ] && [ "$L_STOP" = yes ]; then log "- **U-8: PASS**"; else log "- **U-8: FAIL**"; fi
spike_rm asb-spike-u8
log ""

########################################
log "## U-9 exec クライアントの SIGKILL 後のゲスト内プロセス"
spike_rm asb-spike-u9
spike_create asb-spike-u9
container start asb-spike-u9 >/dev/null
container exec asb-spike-u9 sh -c 'sleep 30; echo done > /root/u9' >/dev/null 2>&1 &
CPID=$!
sleep 2
kill -9 "$CPID" 2>/dev/null
wait "$CPID" 2>/dev/null
sleep 1
GUEST=$(container exec asb-spike-u9 sh -c 'ps | grep "sleep 30" | grep -v grep | wc -l')
log "- クライアント SIGKILL 1 秒後、ゲスト内の 'sleep 30' プロセス数: $GUEST"
spike_rm asb-spike-u9
log ""

########################################
unmanaged_snapshot > "$OUT/unmanaged-after.json"
if cmp -s "$OUT/unmanaged-before.json" "$OUT/unmanaged-after.json"; then
  log "## 管理対象外コンテナの前後比較: 一致"
else
  log "## 管理対象外コンテナの前後比較: **不一致**"
  diff "$OUT/unmanaged-before.json" "$OUT/unmanaged-after.json" | tee -a "$RESULTS"
fi
cleanup
trap - EXIT
LEFT_C=$(container ls -a --format json | jq -r '.[] | .configuration.id' | grep -c '^asb-spike-')
LEFT_I=$(container image ls --format json | jq -r '.[] | .configuration.name // empty' | grep -c 'asb-spike/')
log "- 後始末後に残った asb-spike- コンテナ: $LEFT_C / asb-spike/ イメージ: $LEFT_I"
