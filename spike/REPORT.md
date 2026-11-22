# Phase 0 実測レポート

- 実施: 2026-11-22T13:13:34Z
- 環境: macOS 27.2.1 arm64、`container` CLI 1.7.1(commit a15b460)、イメージ `alpine:3.22`
- 手順: `bash spike/run.sh`。生データは `spike/out/results.md` と `spike/out/*.txt` / `*.json`
- 管理対象外コンテナ(`buildkit`、`web-linux-g0-reference`)の (ID, 状態, 開始時刻) は実行前後で一致。後始末後に `asb-spike-` コンテナと `asb-spike/` イメージは 0 件。

## 結果一覧

| # | 内容 | 結果 | 要点 |
|---|------|------|------|
| U-1 | stop → start でファイルが残る | PASS | 書き込んだマーカーを再開後に読めた。再開 2 秒後も running |
| U-2 | stop でホストのメモリが解放される | PASS(3/3) | tmpfs に 450 MB 書き込み後の VM プロセスの phys_footprint は 709〜710 MB。stop 後、VM プロセスは 3 回とも消滅。`vm_stat` の空きページは約 8,500〜10,600 → 約 57,000〜57,800(16 KB/page、参考値) |
| U-3 | commit できる | PASS | 稼働中・停止中のどちらも成功。同じ参照名への再 commit も成功(上書き) |
| U-4 | commit イメージから内容を保って再作成できる | PASS(3/3) | 3 世代とも、内容、パーミッション 640、削除したファイル(`/etc/motd`)が復活しないこと、ディレクトリが一致。3 巡の書き込み履歴 `gen1 gen2 gen3` が残った |
| U-5 | internal ネットワークで外向き通信が遮断される | PASS | `1.1.1.1:443` への接続は internal で失敗(終了コード 1)、既定で成功(0)。internal では名前解決も失敗 |
| U-6 | 起床などの所要時間 | 記録 | 下表 |
| U-7 | 並行操作 | PASS | 同一コンテナへの並行 exec 2 本、別コンテナへの並行 start 2 本がすべて成功 |
| U-8 | ラベルを取得できる | PASS | 稼働中・停止中のどちらも `configuration.labels` に現れる |
| U-9 | exec クライアントを SIGKILL した後のゲスト内プロセス | 記録 | クライアントを SIGKILL しても、ゲスト内のプロセスは動き続ける |
| U-10 | stop の所要時間 | 記録 | 中央値 138.5 ms。既定の猶予 5 秒は待たない |

### 所要時間(初回を除く 10 回、ms)

| 操作 | 中央値 | 最大 | 最小 |
|------|-------:|-----:|-----:|
| 起床(stopped → start → 最初の exec 成功) | 533 | 570 | 499 |
| 停止(`container stop`) | 138.5 | 151 | 104 |
| archived からの復元(create + start + 最初の exec 成功) | 636 | 707 | 588 |

## ゲート判定

- U-1、U-2、U-5、U-7、U-8: すべて PASS。実装に進む。
- archived の方式: U-3 と U-4 が PASS したため `commit` 方式を採用する。停止中のコンテナに直接 `commit` できるので、計画 3.4 の「稼働させてから commit」経路は使わない。
- U-6: 起床の中央値は 533 ms で、5 秒を大きく下回る。既定のアイドル時間(5 分)は見直さない。
- U-10: `--init` で SIGTERM が転送され、stop は約 0.14 秒で終わる。`stop_timeout_secs` は設定項目として残すが、既定は `container` の既定(5 秒)に任せる。

## 実装に反映する発見

1. **コンテナ内での `mount` には CAP_SYS_ADMIN が必要**。初回の U-2 はこれで失敗した(450 MB を書けず phys_footprint 258 MB)。ツール本体の動作には影響しない。測定スクリプトだけ U-2 のコンテナに `--cap-add CAP_SYS_ADMIN` を付けた。
2. **commit したイメージには、コンテナのラベルが image config の `Labels` として入る**(`asb.spike=1` が残っていた)。Entrypoint `sleep`、Cmd `infinity`、`User 0:0`、`WorkingDir /`、`Env` は PATH のみで、元の `alpine:3.22` と同じ。asb は作成時に必ずエントリポイントとラベルを指定するので、問題にならない。commit したイメージは単一レイヤー(約 8.8 MB)。
3. **internal ネットワークからホスト側のゲートウェイ(192.168.128.1)へは到達できる**(ping 成功)。internal は「外部へ出られない」であって「ホストにも届かない」ではない。README に明記する。
4. **exec の実行中に stop すると、exec は終了コード 137 で終わる**。降格が実行中の exec を殺すと、こうなる。AC-4 の保護がこの事態を防ぐ。
5. **exec クライアントを SIGKILL しても、ゲスト内のプロセスは残る**(U-9)。CLI を強制終了するとリースは解放されるので、ゲスト内の処理が動いていても降格の対象になる。README に既知の制約として書く。
6. イメージ参照名は `docker.io/` が補われる(例: `asb-spike/u4:g2` → `docker.io/asb-spike/u4:g2`)。退避イメージの完全一致判定は、この正規化を考慮する必要がある。

## 測定していない事項

- ホスト再起動後に stopped のコンテナが残るか(`container system stop` を使えないため)。
- commit したイメージから作ったコンテナが、イメージの `Labels` をコンテナのラベルとして引き継ぐか。US-003 で確認する。
