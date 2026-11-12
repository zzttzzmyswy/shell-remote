#!/bin/bash
# One-command release: tag + dist build + GitHub release, all driven by the
# single version source (Cargo.toml). Solves the version-number bug class where
# the release tag and the binary's self-reported version (env!("CARGO_PKG_VERSION"))
# drift apart — the tag is derived from Cargo.toml, and every built binary carries
# exactly that version, so admin panel, agent logs and release assets always agree.
#
# 顺序是**先验证、后动手**：所有会留下外部痕迹的步骤（打 tag / 强推 tag /
# 建 release）都排在全部前置检查之后。任何一项前置检查失败都在动手之前退出，
# 因此失败不会留下"tag 已经指向新 commit、release 却没建出来"的半成品状态
# （旧版把 tag 强推放在 `gh release create` 之前，release 失败时远端 tag 已经
# 被改掉了，重跑还会因为同名 release 已存在而继续失败）。
#
# Usage:  tools/release.sh [--skip-build]
#   --skip-build   用已构建的 dist/ 直接打 tag + 发布（不触发四平台重建）
#   未知参数直接报错退出（旧版静默忽略，`--skip-build` 从未真正生效）。
# Env:    GH_TITLE  覆盖 release 标题（默认从版本派生）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

usage() { sed -n '8,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

SKIP_BUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数: $1（-h 看用法）" >&2; exit 2 ;;
  esac
done

# 版本唯一源：Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$VERSION" ]; then
  echo "无法从 Cargo.toml 读出 version" >&2; exit 1
fi
TAG="v$VERSION"
echo "== 发布 $TAG（版本源: Cargo.toml） =="

# ── 前置检查：全部在动任何 tag / release 之前完成 ────────────────────

# 1) 干净工作树。旧版只 WARN 就继续：未提交的改动不会进 tag，发布出来的
#    二进制与 tag 指向的源码不一致——正是本脚本要消灭的那类版本漂移。
if [ -n "$(git status --porcelain)" ]; then
  echo "工作树有未提交改动（$(git status --porcelain | wc -l) 项），拒绝发布：" >&2
  git status --porcelain >&2
  echo "先提交（或 stash）再发布——tag 只含已提交内容。" >&2
  exit 1
fi

# 2) 已有同名 tag（本地或远端）→ 停止。绝不 `tag -f` / `push -f`：
#    强制移动已发布 tag 会让"某个版本的字节"与"tag 指向的源码"永久失配，
#    且已经下载过旧资产的人无法感知。要重发就 bump 版本号。
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "本地已存在 tag $TAG，拒绝覆盖。发新版本请先 bump Cargo.toml 的 version。" >&2
  exit 1
fi
if [ -n "$(git ls-remote --tags origin "refs/tags/$TAG" 2>/dev/null)" ]; then
  echo "远端已存在 tag $TAG，拒绝覆盖。发新版本请先 bump Cargo.toml 的 version。" >&2
  exit 1
fi

# 3) 已有同名 release → 停止（`gh release create` 会 422，但那时 tag 已经推出去了）。
if gh release view "$TAG" >/dev/null 2>&1; then
  echo "已存在 release $TAG，拒绝重复发布。发新版本请先 bump Cargo.toml 的 version。" >&2
  exit 1
fi

# ── 构建（可跳过）────────────────────────────────────────────────────
#
# 构建必须发生在**资产齐全性检查之前**：默认路径的职责就是产出这些资产，
# 先查后建会把正常的新版本发布也挡下来。
if [ "$SKIP_BUILD" -eq 0 ]; then
  echo "== 构建四平台制品（tools/build-dist.sh）=="
  if [ ! -x tools/build-dist.sh ]; then
    echo "tools/build-dist.sh 不可执行（或不存在），无法构建制品" >&2; exit 1
  fi
  tools/build-dist.sh
fi

# 4) 制品齐全。缺任何一个都直接失败，不能带着不完整的资产发 release。
ASSETS=(
  dist/shell-remote-x86_64
  dist/shell-remote-aarch64
  dist/shell-remote-armv7
  dist/shell-remote-x86_64.exe
  dist/shell-remote-ui-x86_64
  dist/shell-remote-ui-aarch64
  dist/shell-remote-ui-armv7
  dist/shell-remote-ui-x86_64.exe
)
missing=0
for f in "${ASSETS[@]}"; do
  if [ ! -f "$f" ]; then
    echo "缺少制品: $f" >&2
    missing=1
  fi
done
if [ "$missing" -ne 0 ]; then
  echo "dist/ 产物不完整（需 CLI + UI 双二进制 × 四平台），请先跑 tools/build-dist.sh" >&2
  exit 1
fi

# 5) 打 tag 前明确记下要发布的**确切 commit**，发布后便于对账。
COMMIT="$(git rev-parse HEAD)"
echo "目标 commit: $COMMIT"

# ── 副作用从这里开始 ────────────────────────────────────────────────
TITLE="${GH_TITLE:-v$VERSION — shell-remote}"

# NOTES 必须**先**构造好：旧版把 heredoc 直接塞进 `gh release create` 的参数
# 列表里，续行会让资产路径被当成 notes 的一部分（实测 `gh release create`
# 收到的 notes 后面拼着一整串 ./dist/... 路径，资产反而没传上去）。
NOTES="$(cat <<EOF
## $TAG

版本号来自 Cargo.toml（$VERSION），与二进制内部自报版本一致。

四平台双二进制：
- \`shell-remote-<arch>\`（CLI：relay + agent 仅终端转发，无桌面依赖）
- \`shell-remote-ui-<arch>\`（UI：agent 终端 + 桌面转发）

> 桌面转发的设备请改用 \`shell-remote-ui-*\` 二进制（安装/升级均按此文件名）。
EOF
)"

REPO_SLUG="$(git remote get-url origin | sed -E 's#.*[:/]([^/]+/[^/]+?)(\.git)?$#\1#')"

# tag 由本脚本自己创建并推送（而不是交给 `gh release create --target`）：
# 这样"哪个 commit 被打了 tag"是本脚本的确定性结果，发布后可以直接
# `git rev-parse` 对账，不依赖 gh 的隐式行为。
git tag -a "$TAG" -m "release $TAG" "$COMMIT"
if ! git push origin "refs/tags/$TAG"; then
  echo "推送 tag $TAG 失败，回滚本地 tag" >&2
  git tag -d "$TAG" >/dev/null 2>&1 || true
  exit 1
fi

if ! gh release create "$TAG" \
  --repo "$REPO_SLUG" \
  --verify-tag \
  --title "$TITLE" \
  --notes "$NOTES" \
  "${ASSETS[@]}"; then
  # 回滚：release 没建出来，本地与远端 tag 都要撤掉。否则重跑会撞上
  # "已存在 tag"，运维只能手工清理——旧版正是这样卡住的。
  echo "gh release create 失败，回滚 tag $TAG（本地 + 远端）" >&2
  git push origin ":refs/tags/$TAG" >/dev/null 2>&1 || \
    echo "警告: 远端 tag $TAG 回滚失败，请人工删除" >&2
  git tag -d "$TAG" >/dev/null 2>&1 || true
  exit 1
fi

echo "== 已发布: https://github.com/$REPO_SLUG/releases/tag/$TAG (commit $COMMIT) =="
echo "== 自检: 二进制应报版本 v$VERSION =="
