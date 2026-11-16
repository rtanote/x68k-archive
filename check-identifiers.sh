#!/usr/bin/env bash
# 公開リポジトリに個人環境の識別子が混入していないか検査する。
#
# 既存の CI には「API キー」「JASRAC 番号」「禁止ファイル」の検査はあったが、
# **公開ファイルの本文に個人の環境情報を書いてしまう**種類の漏れは検出できなかった。
# それを塞ぐための検査。
#
# 設計: 具体的な固有名 (ホスト名・tailnet 名など) を**このファイルに書かない**。
#       書いてしまうと、この検査自体が固有名の一覧になってしまう。
#       代わりに「形」で検出し、意図的なプレースホルダと公開 ID だけを
#       一般語の許可リストで除外する。
#
# **この検査が見られないもの (push 前に人が意味で見る必要がある)**
#
# 形に一致しないので素通りする。実際に全部やってしまったことがある。
#
#   1. 本番の規模・インフラの数値。索引の件数、サーバの空き容量、ストレージの
#      種類など。環境固有だが形は持たない
#   2. 所有ディスクイメージ名とその内部パス。何を持っているかを明示してしまう
#   3. **アーカイブ本文の逐語引用。** 実測ログからのコピペが混入しやすい。
#      テストのフィクスチャは**同じバイト長の合成文字列**を作ること。
#      ただし実測結果の表に合成文字列を入れてはいけない (測定の捏造になる)。
#      引用をやめて「長さと結果」の表にすれば情報量は落ちない
#
# 使い方:
#   ./check-identifiers.sh                  # 追跡ファイル + origin/main..HEAD のメッセージ
#   ./check-identifiers.sh <base>..<head>   # メッセージの範囲を明示 (CI 用)

set -uo pipefail
cd "$(dirname "$0")"

FOUND=0

report() {
  FOUND=1
  printf '\n[%s]\n%s\n' "$1" "$2"
}

# 追跡ファイルのみを対象にする (= 公開されるもの)。
#
# 除外するもの:
#   - Cargo.lock : 機械生成でクレート名が大量に入り形での誤検出が多い。
#                  識別子が入る経路は無い。
#   - このスクリプト自身 : 検出パターンとして IP レンジ等を含むため、
#                  対象に含めると必ず自分自身を検出してしまう (自己参照)。
files() { git ls-files -- . ':!Cargo.lock' ':!check-identifiers.sh'; }

# ---- 検査項目 ----
#
# **ファイルとコミットメッセージの両方に同じものを当てる。**
# 以前、ファイルだけ綺麗でメッセージに環境情報が残る状態になった
# (スクリプトがメッセージを見ていなかった)。パターンを 1 箇所に置いて
# 両方から使うことで、片方だけ直す事故を防ぐ。

ITEMS=(TAILNET RFC1918 HOMEDIR EMAIL HOSTSHAPE CGNAT)

LABEL_TAILNET='tailnet ホスト名らしき文字列'
PAT_TAILNET='[A-Za-z0-9-]+\.ts\.net'
# 許可: ドキュメント中の明示的なプレースホルダのみ (一般語)
ALLOW_TAILNET='your-tailnet|\bexample\b|<tailnet>|<machine>|<host>|myhost'

LABEL_RFC1918='プライベート IP アドレス'
# 127.0.0.1 / 0.0.0.0 / localhost は公開設定例として正当なので対象外。
PAT_RFC1918='\b(192\.168\.[0-9]{1,3}\.[0-9]{1,3}|10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})\b'
ALLOW_RFC1918=''

LABEL_HOMEDIR='実ユーザのホームディレクトリパス'
PAT_HOMEDIR='/(home|Users)/[A-Za-z][A-Za-z0-9_-]*'
# 許可: プレースホルダ表記のみ
ALLOW_HOMEDIR='/home/<|/Users/<|/home/\$|/Users/\$|/home/USER|/Users/USER|yourname|youruser|your-name|/Users/me'

LABEL_EMAIL='メールアドレス'
PAT_EMAIL='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
# 許可: GitHub の noreply (公開 ID) と anthropic の noreply
ALLOW_EMAIL='@users\.noreply\.github\.com|noreply@anthropic\.com|@example\.(com|org|net)|user@host'

LABEL_HOSTSHAPE='個人機のホスト名らしき文字列'
# `<語>-<語>-ws` のような命名。サフィックスは機材を示す語に絞る
# (`pc` や `host` は一般語に当たりすぎるため含めない)。
PAT_HOSTSHAPE='\b[a-z][a-z0-9]*-[a-z0-9]+-(ws|srv|nas)\b'
ALLOW_HOSTSHAPE=''

LABEL_CGNAT='Tailscale (CGNAT) の IP アドレス'
PAT_CGNAT='\b100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}\b'
ALLOW_CGNAT=''

# 許可リストが空なら素通しする (空パターンの grep は全行に一致してしまう)。
sift() {
  local allow="$1"
  if [ -n "$allow" ]; then
    grep -viE "$allow" || true
  else
    cat
  fi
}

scan_files() {
  files | xargs -r grep -InE "$1" 2>/dev/null | sift "$2"
}

scan_text() {
  grep -nE "$1" 2>/dev/null | sift "$2"
}

# ---- 追跡ファイル ----
for item in "${ITEMS[@]}"; do
  pat="PAT_$item"; allow="ALLOW_$item"; label="LABEL_$item"
  HITS=$(scan_files "${!pat}" "${!allow}")
  [ -n "$HITS" ] && report "追跡ファイル: ${!label}" "$HITS"
done

# ---- コミットメッセージ ----
#
# 公開されるのはファイルだけではない。メッセージは GitHub の PR 画面と
# `git log` に残り、force push しても到達不能オブジェクトとして読める。
RANGE="${1:-}"
if [ -z "$RANGE" ] && git rev-parse --verify -q origin/main >/dev/null 2>&1; then
  RANGE='origin/main..HEAD'
fi

if [ -z "$RANGE" ]; then
  printf '注意: コミットメッセージの検査を飛ばした (比較先が無い)\n'
elif ! git rev-parse --verify -q "${RANGE%%..*}" >/dev/null 2>&1; then
  printf '注意: コミットメッセージの検査を飛ばした (%s を解決できない)\n' "${RANGE%%..*}"
else
  MSGS=$(git log --format='%h %s%n%b' "$RANGE" 2>/dev/null || true)
  if [ -z "$MSGS" ]; then
    printf 'コミットメッセージ: %s に対象なし\n' "$RANGE"
  else
    for item in "${ITEMS[@]}"; do
      pat="PAT_$item"; allow="ALLOW_$item"; label="LABEL_$item"
      HITS=$(printf '%s\n' "$MSGS" | scan_text "${!pat}" "${!allow}")
      [ -n "$HITS" ] && report "コミットメッセージ ($RANGE): ${!label}" "$HITS"
    done
  fi
fi

if [ "$FOUND" -ne 0 ]; then
  printf '\n個人環境の識別子が含まれています。\n'
  printf 'プレースホルダ (例: <host>.<tailnet>.ts.net) に置き換えてください。\n'
  printf '意図的な記載であれば check-identifiers.sh の許可リストに\n'
  printf '一般語として追加してください (固有名そのものは書かないこと)。\n'
  printf 'コミットメッセージ側で見つかった場合は、push 前なら履歴を作り直せます。\n'
  exit 1
fi

echo 'OK: 個人環境の識別子は見つかりませんでした'
