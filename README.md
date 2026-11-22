<img src="assets/icon.svg" width="64" height="64" alt="">

# asb

Apple Container のサンドボックスを、アイドル時間に応じて **hot → stopped → archived** へ自動で降格し、`asb exec` を合図に復元する常駐ツール。AI コーディングエージェントのコマンド実行先を 1 台の Mac に多数置いても、使っていないサンドボックスがメモリを占有し続けないようにする。

考え方は [Sandstorm](https://53able.github.io/docs-site/docs/sandstorm.html)(アイドルなサンドボックスを段階的に降格し、アクセスを合図に起こす Linux 向けデーモン)から借りている。Apple Container の公開 CLI にはメモリ状態を保ったまま退避する手段が無いため、段階の中身は Sandstorm と異なる(下記)。

- 対応環境: Apple silicon の macOS、Apple `container` CLI 1.7 系
- 実装: Rust の単一バイナリ。Apple Container の操作は `container` CLI の子プロセス呼び出しのみ
- 権限: ユーザー権限で動く。root は不要

## 段階

| 段階 | 状態 | 次の段階へ | 復元 |
|------|------|------------|------|
| hot | 稼働中 | アイドル `idle_stop_secs`(既定 300 秒)で stopped | - |
| stopped | `container stop` 済み。ファイルシステムは残る | アイドル `idle_archive_secs`(既定 86400 秒)で archived | 次の exec で `container start`(実測 中央値 0.53 秒) |
| archived | `container commit` でイメージ化し、コンテナを削除 | - | 次の exec で再作成 + 起動(実測 中央値 0.64 秒) |

**降格すると、サンドボックス内のプロセスとメモリは失われる。** 残るのはファイルシステムだけ。バックグラウンドで動かしていたプロセス(dev サーバーなど)は止まる。

アイドル時間は「最後に exec / cp を受け付けた時刻、または最後の exec / cp が終わった時刻」から数える。実行中の exec / cp があるサンドボックスは降格しない。

## 使い方

```sh
cargo build --release
cp target/release/asb ~/.local/bin/   # PATH の通った場所へ

asb daemon &                          # デーモン(フォアグラウンドで動く。ログは標準エラー)

asb create box1 --image alpine:3.22 --memory 512          # 既定は外向き通信なし
asb create box2 --image alpine:3.22 --internet            # インターネット可
asb exec box1 -- sh -c 'echo hello > /root/f'
asb exec -it box1 -- sh                                   # 対話シェル
asb cp ./local.txt box1:/root/local.txt
asb cp box1:/root/f ./f
asb ls                                                    # 一覧(--json で JSON)
asb rm box1                                               # コンテナと退避イメージを削除
```

`asb exec` はサンドボックス内のコマンドの終了コードをそのまま返す。

| 終了コード | 意味 |
|-----------:|------|
| 69 | デーモンに接続できない(`asb daemon` で起動する) |
| 75 | 受付判定で拒否された(hot の割当メモリ合計が上限を超え、止められる hot も無い) |
| 1 | その他の asb のエラー |

### 作成時のオプション

- `--image <ref>`(必須)。イメージは `sleep infinity` を実行できる必要がある。asb はエントリポイントを `sleep infinity` に固定し、`--init` を付けて常駐させる。イメージ既定のコマンドは使わない。
- `--memory <MB>`(既定 1024)、`--cpus <N>`
- `--internet`: インターネットへの外向き通信を許可する。付けなければ `asb-internal`(`container network create --internal`)に接続する。
- 環境変数と作業ディレクトリは作成時ではなく `asb exec -e K=V -w <dir>` で渡す。

### 設定

既定値 → 設定ファイル → 環境変数 の順で上書きする。

| 項目 | 環境変数 | 既定 |
|------|----------|------|
| 設定ディレクトリ | `ASB_HOME` | `~/.asb` |
| 設定ファイル | `ASB_CONFIG` | `<ASB_HOME>/config.json` |
| stopped への降格 | `ASB_IDLE_STOP_SECS` | 300 |
| archived への降格 | `ASB_IDLE_ARCHIVE_SECS` | 86400 |
| 監視周期 | `ASB_TICK_SECS` | 5 |
| hot の割当メモリ合計の上限 | `ASB_MAX_HOT_MEMORY_MB` | 物理メモリの 1/2 |
| `container stop --time` | `ASB_STOP_TIMEOUT_SECS` | `container` の既定 |
| ソケット | `ASB_SOCKET` | `<ASB_HOME>/asb.sock` |
| DB | `ASB_DB` | `<ASB_HOME>/asb.db` |

設定ファイルの例:

```json
{ "idle_stop_secs": 600, "idle_archive_secs": 172800, "max_hot_memory_mb": 8192 }
```

## 仕組み

- デーモンは SQLite に「あるべき状態」(作成時の引数、最終アクセス時刻、退避イメージの参照名、実行中の exec のリース)を持ち、実際の状態は毎回 `container ls -a --format json` から読む。
- 管理対象はラベル `asb.managed=true` で識別する。このラベルを持たないコンテナ(例: `buildkit`)は一覧にも出さず、停止・削除・commit もしない。破壊的操作の直前に毎回ラベルを確認する。
- `asb exec` はデーモンからリースを取り(必要ならここで復元)、CLI 自身が `container exec` を起動する。リースは CLI と `container exec` の PID・開始時刻とともに SQLite に保存され、両方のプロセスが終わるまで有効。デーモンを再起動しても実行中の exec は降格から守られる。
- 受付判定: hot を増やす操作(作成・起床)の前に、hot の割当メモリ合計 + 要求量 が上限を超えないかを判定する。超える場合は、実行中の exec が無い hot を最終アクセスの古い順に停止して空きを作る。それでも足りなければ拒否する(終了コード 75)。
- archived の手順: `commit` → イメージの存在確認 → DB に確定 → コンテナ削除 → 旧世代のイメージ削除。途中で失敗したらコンテナを残し、次の周期で再試行する。退避イメージは `asb-archive/<name>:g<世代>`。

## 既知の制約

- **デーモンを経由しない `container exec` はアイドル判定に入らない。** そのサンドボックスは実行中でも降格されうる(降格された exec は終了コード 137 で終わる)。
- **リースはホスト側のプロセス(`asb exec` の CLI と、その子の `container exec`)の生存で判定する。ゲスト内のプロセスは見ていない。**
  - CLI だけを強制終了(SIGKILL)した場合、子の `container exec` が動いている間はリースが残り、降格されない。
  - `container exec` も終了すると、ゲスト内の処理が動き続けていてもリースは解放され、降格の対象になる(Phase 0 の実測で、`container exec` のクライアントを kill してもゲスト内プロセスは残ることを確認)。
  - CLI が `container exec` を起動してからデーモンへの登録(Attach)を終えるまでの間に CLI が強制終了された場合、子の `container exec` はリースに記録されていないので保護されない。
- **internal ネットワークはホストへは到達できる。** 外部(インターネット)へは出られないが、ゲートウェイ(ホスト)のサービスには届く。アドレス単位の allow / deny は無い。
- 受付判定は割当メモリ(`--memory`)の合計だけで判断する。VM ごとのオーバーヘッドは数えない。
- 起床はコールドブート(プロセスは新しく始まる)。
- 退避イメージの削除は `asb rm` と次回の降格時に行う。古い退避イメージの自動削除(期限切れ)は無い。
- デーモンの常駐は手動起動。LaunchAgent の設定は同梱していない。

## Non-Goals

- プロセスやメモリの状態を保ったままの退避(Sandstorm の frozen / snapshot 相当)
- TCP / HTTP 接続を合図にした起床、公開ポートのプロキシ
- HTTP API、SDK、MCP サーバー、Web コンソール
- 複数ノード構成、マルチテナント、認証
- アドレス単位の egress 制御、ホストの pf 操作
- 実行中サンドボックスのチェックポイントからの分岐
- Linux、Intel Mac、`container` 1.7 系以外

## 前提の実測(Phase 0)

設計の前提は `spike/run.sh` で実測した。詳細は [spike/REPORT.md](spike/REPORT.md)。

| 前提 | 結果 |
|------|------|
| stop → start でファイルが残る | 成立 |
| stop で VM プロセスが消え、ホストのメモリが戻る | 成立(450 MB 書き込んだ VM の phys_footprint 約 710 MB が、stop で VM プロセスごと消える。3/3) |
| 停止中・稼働中のコンテナを commit でき、内容・パーミッション・削除済みファイルを保って再作成できる | 成立(3 世代) |
| `--internal` ネットワークから外部へ出られない | 成立(ホストのゲートウェイには届く) |
| 同一コンテナへの並行 exec、別コンテナへの並行 start | 成立 |
| ラベルを稼働中・停止中のどちらでも取得できる | 成立 |

| 操作(初回を除く 10 回) | 中央値 | 最大 |
|--------------------------|-------:|-----:|
| 起床(stopped → 最初の exec) | 533 ms | 570 ms |
| 停止 | 139 ms | 151 ms |
| archived からの復元(→ 最初の exec) | 636 ms | 707 ms |

### 未検証の事項

- ホスト再起動後に stopped のコンテナが残るか
- `container` 1.7 系以外での動作

## テスト

```sh
cargo test                 # 単体テスト(実機不要)
sh scripts/e2e.sh          # 実機 e2e(12 本)+ 管理対象外コンテナの前後比較
```

e2e は `alpine:3.22` を使い、ネットワークのテストで `1.1.1.1:443` に接続する。テストが作るコンテナとイメージは名前接頭辞 `asb-e2e-` を持ち、終了時に削除される。

## ライセンス

Apache-2.0
