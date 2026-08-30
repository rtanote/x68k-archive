# Server Deployment Guide (Linux + Tailscale Funnel + Claude Desktop)

xdf-mcp を **常駐 Linux サーバ** に配置し、Tailscale Funnel 経由の HTTPS で
Claude Desktop の Custom Connector から利用するためのセットアップ手順。

## 1. このガイドのカバー範囲

```
[Claude Desktop on Mac/Win/iPad]
        ↓ HTTPS (Anthropic backend が接続)
[<machine>.<tailnet>.ts.net (Tailscale Funnel)]
        ↓ Tailscale 内部
[Linux サーバ:8765]
        ↓ Docker (xdf-mcp HTTP モード)
[名前付きボリューム /index] + [bind mount /archive (NFS マウント)]
        ↓ LAN
[NAS (Synology 等) の共有フォルダ]
```

このガイドが想定する状況:

- 24h 稼働の Linux マシン (ミニ PC / NUC / 自宅サーバ等)
- アーカイブイメージ群を NAS に置いて LAN マウント
- Mac / Windows / iPad の **Claude Desktop** から自然言語で検索したい
- 自分専用 (個人運用)。一般公開は想定しない (URL 秘密路で実用十分なため)

## 2. 前提条件

| 項目 | 必要なもの | 推奨 |
|---|---|---|
| OS | Linux (kernel 5.x+) | Ubuntu 22.04 LTS 以降 |
| Docker | 24.x+ | Docker Engine + Compose v2 |
| Tailscale | 1.50+ | Funnel 機能対応版 |
| ストレージ | LAN-mountable NAS (NFS or SMB) | 数 TB の空き容量 |
| ネットワーク | 自宅ルータの NAT 越え (Tailscale 任せ) | UPnP 不要 |
| API キー | Anthropic API (要約バッチ使う場合) | https://console.anthropic.com/ |

## 3. NAS の共有フォルダを Linux にマウント (NFS)

### Synology DSM 側
1. Control Panel → File Services → **NFS** タブで「NFS サービスを有効化」
2. Control Panel → Shared Folder → 対象フォルダ → **NFS Permissions** で Linux サーバの IP を許可
   - Privilege: **Read-only** (アーカイブ用)
   - 表示される **Mount path** を控える (`/volume1/<share>`)

### Linux 側
```bash
sudo apt install nfs-common avahi-daemon libnss-mdns
sudo systemctl enable --now avahi-daemon
sudo mkdir -p /mnt/nas/archive

# 動作テスト
sudo mount -t nfs4 <nas-host>:/volume1/<share> /mnt/nas/archive -o ro

# 永続化 (/etc/fstab)
echo "<nas-host>:/volume1/<share>  /mnt/nas/archive  nfs4  ro,noatime,_netdev,nofail,noauto,x-systemd.automount,x-systemd.mount-timeout=30,x-systemd.requires=avahi-daemon.service  0 0" \
  | sudo tee -a /etc/fstab
sudo systemctl daemon-reload
```

> `<nas-host>` は固定 IP か `.local` 名 (mDNS)。後者なら avahi-daemon が必要。

### なぜ `x-systemd.automount` を付けるか (重要)

`nofail` だけだと、**NAS が停止している間にサーバが再起動した場合にマウントされず、
NAS を復帰させても自動では繋がらない** (手動 `sudo mount` が必要)。
`noauto,x-systemd.automount` にしておくと、起動時に固定マウントせず
**パスへのアクセス時に自動マウント**するため、NAS 復帰後の初回アクセスで自然に繋がる。

`x-systemd.idle-timeout` は**付けないこと**。アイドルで自動 umount されると
NFS の付け外しが繰り返され、無駄な再マウントを誘発する。

適用確認:
```bash
systemctl list-units --type=automount --all | grep <mountpoint>
systemctl show <mnt-path>.automount -p WantedBy   # remote-fs.target になっていること
```

> `daemon-reload` は既存のマウントを umount しない。稼働中でも安全に適用できる
> (automount が実際に効くのは次回起動時)。

確認:
```bash
ls /mnt/nas/archive
df -h /mnt/nas/archive
```

## 4. プロジェクトの配置

```bash
git clone https://github.com/<owner>/xdf-fs.git ~/xdf-fs
cd ~/xdf-fs
```

`.env` を編集:
```bash
cp .env.example .env
nano .env
```

```ini
ANTHROPIC_API_KEY=sk-ant-...        # AI 要約使う場合のみ
ARCHIVE_DIR=/mnt/nas/archive
HTTP_PORT=8765
```

ビルド:
```bash
docker compose build
```

Rust コンパイルが入るので初回は 5〜10 分かかる。

## 5. Tailscale + Funnel セットアップ

### 5.1 Tailscale クライアント
```bash
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up
```

URL が表示されるのでブラウザで承認。**同じアカウントで** Mac / Win / iPad
側にも Tailscale を入れておく (Tailnet は自動共有)。

### 5.2 Funnel を Tailnet で有効化
管理コンソール (https://login.tailscale.com/admin) でアカウント単位の許可が
必要。コマンド初回実行時に承認 URL が出るのでそれで OK。

### 5.3 Funnel で 8765 ポートを公開
```bash
sudo tailscale funnel --bg --https=443 http://localhost:8765
tailscale funnel status
```

期待出力:
```
# Funnel on:
#     - https://<machine>.<tailnet>.ts.net
https://<machine>.<tailnet>.ts.net (Funnel on)
|-- / proxy http://localhost:8765
```

`<machine>.<tailnet>.ts.net` がベース URL になる。

> Funnel は **公開インターネット** に晒される。後述の URL 秘密路 (Section 6)
> で防御するため、必ずそのセットで運用する。

## 6. URL 秘密路の設定 (重要)

Claude Desktop の Custom Connector は **OAuth 2.0 のみ対応** で、Bearer
token のような単純な認証ヘッダ機能が無い。代わりに **URL パス自体を
長いランダム文字列にする** ことで、URL を知る = 認証 とする運用を採用する。

### 6.1 秘密パスの生成
```bash
# 16 バイト hex (32 文字) を生成
SECRET=$(openssl rand -hex 16)
echo "Generated secret path: /${SECRET}/mcp"

# .env に書き込み
echo "XDF_MCP_HTTP_PATH=/${SECRET}/mcp" >> .env
```

### 6.2 Host ヘッダ allowlist 設定
rmcp の DNS rebinding 防御で、Funnel 経由のホスト名も許可する必要あり:
```bash
echo "XDF_MCP_ALLOWED_HOSTS=<machine>.<tailnet>.ts.net" >> .env
```

### 6.3 起動 (or 再起動)
```bash
docker compose up -d
docker compose logs xdf-mcp | tail -5
```

期待ログ:
```
xdf-mcp: allowed Host headers = localhost, 127.0.0.1, ::1, <machine>.<tailnet>.ts.net
xdf-mcp: NO authentication (set XDF_MCP_AUTH_TOKEN if exposing publicly)
xdf-mcp HTTP listening on http://0.0.0.0:8765/<secret>/mcp
```

### 6.4 動作確認
別端末から:
```bash
# 秘密パス無し → 404
curl -s -o /dev/null -w "%{http_code}\n" https://<machine>.<tailnet>.ts.net/mcp

# 秘密パスあり → 406 (= MCP に到達、Accept ヘッダ不足で拒否されるのが正常応答)
curl -s -o /dev/null -w "%{http_code}\n" "https://<machine>.<tailnet>.ts.net/<secret>/mcp"
```

## 7. アーカイブ索引化と AI 要約

### 7.1 索引ビルド
```bash
docker compose run --rm xdf-mcp xdf-index build /archive --out /index --fresh
docker compose run --rm xdf-mcp xdf-index status --index /index
```

NAS 経由 I/O のため数百 disks で 10〜20 分かかる。完了時に「Indexed: NN
new image(s)」と documents 数が表示される。

### 7.2 (任意) フロッピーグルーピング設定
雑誌付属ディスクのように 1 号が複数枚に分かれている場合、まとめて要約する
ための設定:
```bash
cp xdf-grouping.example.toml xdf-grouping.toml
# 必要に応じて [[rule]] のパターンを編集
```

`show-groups` でプレビュー (API コスト無し):
```bash
docker compose run --rm \
  -v $PWD/xdf-grouping.toml:/cfg/groups.toml:ro \
  xdf-mcp xdf-index show-groups /archive --groups /cfg/groups.toml | tail -10
```

### 7.3 AI 要約バッチ
Anthropic API キー必須。`.env` に `ANTHROPIC_API_KEY` を設定済みであること:
```bash
docker compose run --rm \
  -v $PWD/xdf-grouping.toml:/cfg/groups.toml:ro \
  xdf-mcp xdf-index summarize /archive --index /index \
    --groups /cfg/groups.toml \
    --max-cost-usd 10.0 \
    --rate-sleep 25
```

| フラグ | 用途 |
|---|---|
| `--max-cost-usd N` | ローカル累計コスト上限 (Anthropic 残高とは別の保険) |
| `--rate-sleep N` | API 呼び出し間 sleep (Tier 1 の 30K TPM 対策、Sonnet で 25 秒推奨) |
| `--dry-run` | API 呼ばずプロンプト表示のみ |
| `--force` | 既存サマリも上書き再生成 |

途中失敗 (残高不足や 429) しても、再実行で **既要約は skip**、未処理のみ続行。

## 8. Claude Desktop に Custom Connector として登録

設定 → コネクタ → **カスタムコネクタを追加 (ベータ版)**:

| フィールド | 値 |
|---|---|
| Name | 任意 (例: `x68k-archive`) |
| URL | `https://<machine>.<tailnet>.ts.net/<secret>/mcp` |
| OAuth Client ID | (空欄) |
| OAuth Client Secret | (空欄) |

「追加」 → 「連携/連携させる」を押して **接続済み** になれば成功。

## 9. 運用のヒント

### 9.1 同時利用
索引ビルドや要約バッチは **別コンテナ** (`docker compose run --rm`) で動くので、
常駐 MCP サーバは継続稼働。バッチ走行中も Claude Desktop から検索や読み取り可能。

### 9.2 NAS にファイルを追加した時
```bash
# 索引を増分更新 (--fresh 無し)
docker compose run --rm xdf-mcp xdf-index build /archive --out /index

# 新規ディスクのみ要約
docker compose run --rm \
  -v $PWD/xdf-grouping.toml:/cfg/groups.toml:ro \
  xdf-mcp xdf-index summarize /archive --index /index \
    --groups /cfg/groups.toml --rate-sleep 25
```

### 9.3 サーバ再起動後
docker-compose の `restart: unless-stopped` 設定で自動復活。

**Tailscale Funnel は再起動後も自動で復帰する。** `tailscale funnel --bg` の設定は
tailscaled が永続化しており、手動で立ち上げ直す必要はない
(2026-08-24 に実機で確認済み: 再起動後も `tailscale funnel status` が `on`)。

ただし **Claude Desktop 側は再接続が必要**になる場合がある → Section 10 参照。

### 9.4 NFS マウントとコンテナの関係 (ハマりどころ)

`docker-compose.yml` の archive bind mount には `bind: propagation: rslave` が
必要 (設定済み)。既定の `rprivate` だと **コンテナ起動時点のマウント状態が固定される**ため、
ホスト側で後から `mount` してもコンテナ内の `/archive` は空のまま見える。

```yaml
      - type: bind
        source: ${ARCHIVE_DIR:-./tests/data}
        target: /archive
        read_only: true
        bind:
          propagation: rslave
```

前提はホスト側の該当パスが `shared` 伝播であること (systemd 環境では既定で shared)。
確認と、伝播が効いているかの検証:

```bash
findmnt -T /mnt/nas/archive -o TARGET,PROPAGATION        # → shared
docker exec xdf-mcp grep ' /archive ' /proc/self/mountinfo   # → master:NNN
```

ホスト側が `shared:NNN`、コンテナ側が同じ番号の `master:NNN` になっていれば、
ホストの mount/umount がコンテナへ伝播する (= コンテナ再起動が不要)。

> `propagation` の変更は `docker compose restart` では反映されない。
> **`docker compose up -d --force-recreate`** でコンテナを作り直すこと。

### 9.5 ログ
```bash
docker compose logs xdf-mcp -f             # MCP リクエスト
journalctl -u tailscaled -f                # Tailscale 接続
```

起動バナーには **秘密パスと公開 URL が出力される**ので、URL を見失ったときはここで確認できる:
```bash
docker logs xdf-mcp 2>&1 | grep "HTTP listening"
```

## 10. トラブルシューティング

### 「Couldn't reach the MCP server」/ 「Invalid content from server」

**既定 (stateless) では起きないはず。`--stateful` を付けている場合のみ該当する。**

stateful モードでは Streamable HTTP のセッションが**メモリ上のみ**に保持される。
サーバ再起動 (ホスト再起動を含む) で全セッションが消えるため、クライアントが
保持している古い `Mcp-Session-Id` に対してサーバは **404** と
`Not Found: Session not found` というプレーンテキスト (JSON-RPC ですらない) を
返し続ける。これがクライアント側では

- Claude Desktop: 「Couldn't reach the MCP server」
- リバースプロキシ経由 (claude.ai のコネクタ等):
  `-32600 Anthropic Proxy: Invalid content from server`

として見える。接続もツール一覧も生きているのに**全てのツール呼び出しだけが失敗する**、
引数やレスポンスのサイズを変えても症状が変わらない、というのが特徴。

→ 恒久対策は `--stateful` を外すこと (既定の stateless に戻す)。
   その場でしのぐなら、クライアント側でコネクタを切断 → 再連携 (またはアプリ再起動)。

サーバ側が本当に落ちているかの切り分け:
```bash
# 1. 秘密パス無し → 404、秘密パスあり → 406 なら MCP に到達している
curl -s -o /dev/null -w "%{http_code}\n" "https://<machine>.<tailnet>.ts.net/<secret>/mcp"

# 2. initialize が 200 で返れば完全に正常 (セッションを新規に張れている)
curl -s -o /dev/null -w "%{http_code}\n" -X POST "https://<machine>.<tailnet>.ts.net/<secret>/mcp" \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"diag","version":"1.0"}}}'
```

その他:
- URL に **秘密パス** が含まれているか確認
- `tailscale funnel status` で Funnel が "on" か

> **Tailnet 内から検証するときの注意**: MagicDNS により
> `<machine>.<tailnet>.ts.net` が Tailnet IP (100.x) に解決されるため、
> 素の curl では**公開経路を検証できていない**。公開経路を確かめるには
> ingress IP を明示する:
> ```bash
> dig +short <machine>.<tailnet>.ts.net          # 公開 ingress IP を取得
> curl --resolve "<host>:443:<ingress-ip>" https://<host>/<secret>/mcp ...
> ```

> **Tailnet IP 直打ち (`http://100.x.x.x:8765/...`) が 403 になるのは正常。**
> `XDF_MCP_ALLOWED_HOSTS` に IP が入っていないため rmcp の DNS rebinding 防御が
> 弾いている。これを「壊れている」と誤読しないこと。

### 検索は動くのに読み取りだけ失敗する

**NFS が未マウントの典型症状。** `archive_search` は抜粋を索引内に持つため
アーカイブ実体が無くても正常に応答し続ける。実体が要るのは
`archive_read` / `archive_list` / `archive_view_image` / `archive_extract` のみ。

```bash
mount | grep nfs                                   # ホスト側
docker exec xdf-mcp sh -c 'ls /archive | wc -l'    # コンテナ側 (0 なら NG)
```

復旧:
```bash
sudo mount /mnt/nas/archive     # fstab にエントリがあればマウントポイント指定だけでよい
```

`propagation: rslave` が設定済みならこれだけでコンテナにも伝播する。
未設定 (`rprivate`) の場合は `docker compose up -d --force-recreate xdf-mcp` が別途必要。
恒久対策は Section 3 (`x-systemd.automount`) と Section 9.4 (`rslave`) を参照。

### 「Forbidden: Host header is not allowed」
rmcp の Host allowlist に Tailnet ホスト名が無い:
```bash
echo "XDF_MCP_ALLOWED_HOSTS=<your>.<tailnet>.ts.net" >> .env
docker compose down && docker compose up -d
```

### 索引時に `Suspicious bytes_per_sector` 警告
標準 XDF の BPB ではなく **Hudson DIM 形式** (16 バイト OEM 拡張) のディスクの場合、
xdf-fs は自動 fallback で 2HD 1232KB のデフォルト値を使う。警告のみで実害なし。

### 索引時に `Sector NNN out of range` 警告
ディスクイメージの FAT12 チェーンに不整合あり (実機障害 or ダンプ時の損傷)。
当該 disk のみスキップされる。

### LZH 展開エラー (`unknown header level`)
delharc が対応してない LZH バリアント。当該 LZH 内部のみスキップされる。

### 要約バッチで `429 Too Many Requests`
Anthropic のレート制限 (Tier 1 で 30K input TPM)。`--rate-sleep 25` 以上を指定して再実行。
途中までのサマリはキャッシュ済みなので skip される。

### `Couldn't send tool approval` が時々出る
Tailscale Funnel 経由の長距離経路でセッションがゆらぐと発生。**再試行で復活** する。
頻発するなら `tailscale netcheck` でネットワーク状態を確認。

## 11. 参考リンク

- [Claude Desktop Custom Connectors](https://support.anthropic.com/en/articles/11503834-claude-desktop-mcp-server)
- [Tailscale Funnel](https://tailscale.com/kb/1223/funnel)
- [Synology NFS Setup](https://kb.synology.com/en-us/DSM/help/DSM/AdminCenter/file_share_nfs)
- [MCP Specification](https://spec.modelcontextprotocol.io/)
- [docs/claude-desktop-setup.md](claude-desktop-setup.md) — Claude Desktop での stdio / HTTP MCP 一般設定
