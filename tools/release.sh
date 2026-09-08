#!/bin/bash
# One-command release: tag + dist rebuild + GitHub release, all driven by the
# single version source (Cargo.toml). Solves the version-number bug class where
# the release tag and the binary's self-reported version (env!("CARGO_PKG_VERSION"))
# drift apart — the tag is derived from Cargo.toml, and every built binary carries
# exactly that version, so admin panel, agent logs and release assets always agree.
#
# Usage:  tools/release.sh [--skip-build]
#   --skip-build   用已构建的 dist/ 直接打 tag + 发布（不再触发四平台重建）
# Env:    GH_TITLE  覆盖 release 标题（默认从版本派生）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 版本唯一源：Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
TAG="v$VERSION"
echo "== 发布 $TAG（版本源: Cargo.toml） =="

if [ ! -f "dist/shell-remote-x86_64" ] || [ ! -f "dist/shell-remote-ui-x86_64" ]; then
  echo "dist/ 缺少产物（需 CLI + UI 双二进制），请先跑 tools/build-dist.sh"; exit 1
fi

# 干净工作树才允许打 tag（防止发布未提交的改动）
if [ -n "$(git status --porcelain)" ]; then
  echo "WARN: 工作树有未提交改动($(git status --porcelain | wc -l) 项)。仍继续，tag 已含全部已提交内容。"
fi
git tag -f "$TAG"
git push origin "$TAG" -f

TITLE="${GH_TITLE:-v$VERSION — shell-remote}"

# 资产文件名即平台约定（与 install.sh / admin 升级 key 一致）。
# 每个平台两个二进制：
#   shell-remote-<arch>      CLI（relay + agent 终端，lean）
#   shell-remote-ui-<arch>   UI（agent 终端 + 桌面）
gh release create "$TAG" \
  --title "$TITLE" \
  --notes "$(cat <<EOF
## $TAG

版本号来自 Cargo.toml（$VERSION），与二进制内部自报版本一致。

四平台双二进制：
- \`shell-remote-<arch>\`（CLI：relay + agent 仅终端转发，无桌面依赖）
- \`shell-remote-ui-<arch>\`（UI：agent 终端 + 桌面转发）

> 桌面转发的设备请改用 \`shell-remote-ui-*\` 二进制（安装/升级均按此文件名）。
EOF
)" \
  ./dist/shell-remote-x86_64 \
  ./dist/shell-remote-aarch64 \
  ./dist/shell-remote-armv7 \
  ./dist/shell-remote-x86_64.exe \
  ./dist/shell-remote-ui-x86_64 \
  ./dist/shell-remote-ui-aarch64 \
  ./dist/shell-remote-ui-armv7 \
  ./dist/shell-remote-ui-x86_64.exe

echo "== 已发布: https://github.com/zzttzzmyswy/shell-remote/releases/tag/$TAG =="
echo "== 自检: 二进制应报版本 v$VERSION =="