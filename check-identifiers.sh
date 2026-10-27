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
# 使い方:
#   ./check-identifiers.sh          # 追跡ファイル全体を検査
#   CI からも同じものを呼ぶ

set -uo pipefail
cd "$(dirname "$0")"

FOUND=0

report() {
  FOUND=1
  printf '\n[%s]\n%s\n' "$1" "$2"
}

# 追跡ファイルのみを対象にする (= 公開されるもの)。
# Cargo.lock は機械生成でクレート名が大量に入るため、形による誤検出が多い。
# 識別子が Cargo.lock に入る経路は無いので除外する。
files() { git ls-files -- . ':!Cargo.lock'; }

# ---- 1. tailnet ホスト名 ----
# 許可: ドキュメント中の明示的なプレースホルダのみ (一般語)
HITS=$(files | xargs -r grep -InE '[A-Za-z0-9-]+\.ts\.net' 2>/dev/null \
  | grep -viE 'your-tailnet|\bexample\b|<tailnet>|<machine>|<host>|myhost' || true)
[ -n "$HITS" ] && report "tailnet ホスト名らしき文字列" "$HITS"

# ---- 2. RFC1918 プライベート IP ----
# 127.0.0.1 / 0.0.0.0 / localhost は公開設定例として正当なので対象外。
HITS=$(files | xargs -r grep -InE \
  '\b(192\.168\.[0-9]{1,3}\.[0-9]{1,3}|10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})\b' \
  2>/dev/null || true)
[ -n "$HITS" ] && report "プライベート IP アドレス" "$HITS"

# ---- 3. 実ユーザのホームディレクトリパス ----
# 許可: プレースホルダ表記のみ
HITS=$(files | xargs -r grep -InE '/(home|Users)/[A-Za-z][A-Za-z0-9_-]*' 2>/dev/null \
  | grep -viE '/home/<|/Users/<|/home/\$|/Users/\$|/home/USER|/Users/USER|yourname|youruser|your-name|/Users/me' || true)
[ -n "$HITS" ] && report "実ユーザのホームディレクトリパス" "$HITS"

# ---- 4. メールアドレス ----
# 許可: GitHub の noreply (公開 ID) と anthropic の noreply
HITS=$(files | xargs -r grep -InE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' 2>/dev/null \
  | grep -viE '@users\.noreply\.github\.com|noreply@anthropic\.com|@example\.(com|org|net)|user@host' || true)
[ -n "$HITS" ] && report "メールアドレス" "$HITS"

# ---- 5. 個人機のホスト名らしき形 ----
# `<語>-<語>-ws` のような命名。サフィックスは機材を示す語に絞る
# (`pc` や `host` は一般語に当たりすぎるため含めない)。
HITS=$(files | xargs -r grep -InE '\b[a-z][a-z0-9]*-[a-z0-9]+-(ws|srv|nas)\b' 2>/dev/null || true)
[ -n "$HITS" ] && report "個人機のホスト名らしき文字列" "$HITS"

# ---- 6. Tailscale の IP レンジ (100.64.0.0/10 の CGNAT) ----
HITS=$(files | xargs -r grep -InE '\b100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}\b' 2>/dev/null || true)
[ -n "$HITS" ] && report "Tailscale (CGNAT) の IP アドレス" "$HITS"

if [ "$FOUND" -ne 0 ]; then
  printf '\n個人環境の識別子が公開ファイルに含まれています。\n'
  printf 'プレースホルダ (例: <host>.<tailnet>.ts.net) に置き換えてください。\n'
  printf '意図的な記載であれば check-identifiers.sh の許可リストに\n'
  printf '一般語として追加してください (固有名そのものは書かないこと)。\n'
  exit 1
fi

echo 'OK: 個人環境の識別子は見つかりませんでした'
