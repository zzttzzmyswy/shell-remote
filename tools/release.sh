#!/bin/bash
# One-command release: tag + dist build + GitHub release, all driven by the
# single version source (Cargo.toml). Solves the version-number bug class where
# the release tag and the binary's self-reported version (env!("CARGO_PKG_VERSION"))
# drift apart — the tag is derived from Cargo.toml, and every built binary carries
# exactly that version, so admin panel, agent logs and release assets always agree.
#
# 前置检查完成后才创建 tag / release。发布请求失败时保留 tag，
# 因为远端可能已经创建 release，须核实实际状态后恢复。
#
# Usage:  tools/release.sh [--skip-build]
#   --skip-build   用已构建的 dist/ 直接打 tag + 发布（不触发四平台重建）
#   未知参数直接报错退出（旧版静默忽略，`--skip-build` 从未真正生效）。
# Env:    GH_TITLE  覆盖 release 标题（默认从版本派生）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

usage() { echo 'Usage: tools/release.sh [--skip-build]'; }

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

# 远端查询必须区分「确实不存在」与「查询失败」（MYS-2806 review 2）：
# `ls-remote` 在断网/认证失败时也输出空，直接当"不存在"会在无法确认远端
# 状态的情况下继续发布。
if ! ls_out="$(git ls-remote --tags origin "refs/tags/$TAG" 2>&1)"; then
  echo "无法查询远端 tag 状态（网络或认证失败），为安全起见停止发布：" >&2
  echo "  $ls_out" >&2
  echo "这是**查询失败**，不能据此判断 $TAG 不存在。" >&2
  exit 1
fi
if [ -n "$ls_out" ]; then
  echo "远端已存在 tag $TAG，拒绝覆盖。发新版本请先 bump Cargo.toml 的 version。" >&2
  exit 1
fi

REPO_SLUG="$(git remote get-url origin | sed -E 's#\.git$##; s#.*[:/]([^/]+/[^/]+)$#\1#')"

# 3) 已有同名 release → 停止（`gh release create` 会 422，但那时 tag 已经推出去了）。
#    同样必须区分「确实不存在」与「查询失败」：`gh release view` 对"不存在"和
#    "没登录/网络不通"都会非零退出，后者当作不存在会让发布在未知状态下继续。
#    查询与创建必须针对**同一个仓库**，所以两者都显式带 `--repo`。
VIEW_ERROR="$(mktemp "${TMPDIR:-/tmp}/sr-release-view.XXXXXX")"
trap 'rm -f "$VIEW_ERROR"' EXIT
if ! gh api "repos/$REPO_SLUG" --silent 2>"$VIEW_ERROR"; then
  echo "无法确认仓库访问权限，停止发布：" >&2
  cat "$VIEW_ERROR" >&2
  exit 1
fi
if gh release view "$TAG" --repo "$REPO_SLUG" >/dev/null 2>"$VIEW_ERROR"; then
  echo "已存在 release $TAG，拒绝重复发布。发新版本请先 bump Cargo.toml 的 version。" >&2
  exit 1
fi
if ! grep -qiE '^release not found$|HTTP 404' "$VIEW_ERROR"; then
  echo "无法确认 $TAG 的 release 状态（网络/认证/权限失败），停止发布：" >&2
  sed 's/^/  /' "$VIEW_ERROR" >&2
  echo "这是**查询失败**，不能据此判断该 release 不存在。" >&2
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

# Release notes are one argument, followed by eight independent asset arguments.
NOTES="$(cat <<EOF
## $TAG

版本号来自 Cargo.toml（$VERSION），与二进制内部自报版本一致。

四平台双二进制：
- \`shell-remote-<arch>\`（CLI：relay + agent 仅终端转发，无桌面依赖）
- \`shell-remote-ui-<arch>\`（UI：agent 终端 + 桌面转发）

> 桌面转发的设备请改用 \`shell-remote-ui-*\` 二进制（安装/升级均按此文件名）。
EOF
)"

# tag 由本脚本自己创建并推送（而不是交给 `gh release create --target`）：
# 这样"哪个 commit 被打了 tag"是本脚本的确定性结果，发布后可以直接
# `git rev-parse` 对账，不依赖 gh 的隐式行为。
git tag -a "$TAG" -m "release $TAG" "$COMMIT"
if ! git push origin "refs/tags/$TAG"; then
  echo "推送 tag $TAG 未成功确认；保留本地 tag，请先核对远端状态，不要盲目重跑。" >&2
  exit 1
fi

if ! gh release create "$TAG" \
  --repo "$REPO_SLUG" \
  --verify-tag \
  --title "$TITLE" \
  --notes "$NOTES" \
  "${ASSETS[@]}"; then
  # 失败后**绝不能**盲目删除 tag（MYS-2806 review 2）。
  #
  # `gh release create` 在 GitHub API 上是多个请求：创建 release → 上传资产 →
  # 发布（草稿转正式）→ 清理草稿。失败可能发生在其中**任何一步之后**，因此
  # 非零退出只说明"本次命令失败"，不说明"远端没有 release"。此时删掉远端 tag
  # 会留下"release 存在、tag 却没了"的更糟状态：release 指向的 commit 失去
  # 可追溯引用，而 `gh` 也不会替我们重新打。
  #
  # 正确做法：保留本地与远端 tag（它们指向确切 COMMIT，是唯一可靠的锚点），
  # 报告实际状态与恢复方法，让人工决定；绝不自动覆盖/重发同版本。
  echo >&2
  echo "== 发布未成功完成：gh release create 返回非零 ==" >&2
  echo "   版本: $TAG    commit: $COMMIT" >&2
  echo "   tag 已保留（本地 + 远端），**未**自动删除或覆盖。" >&2
  echo >&2
  echo "   可能的状态与恢复方法：" >&2
  echo "   1) release 已创建、仅资产上传/草稿清理失败（最常见）" >&2
  echo "      → 先确认实际状态：gh release view $TAG --repo $REPO_SLUG" >&2
  echo "      → 若 release 存在，直接补传缺失资产（不要重建同版本）：" >&2
  echo "        gh release upload $TAG <仅缺失且已验证的资产> --repo $REPO_SLUG" >&2
  echo "   2) release 未创建，但 tag 已推送" >&2
  echo "      → 确认 tag 指向本次 commit 且确实没有 release 后，保留 tag 补建 release" >&2
  echo "          gh release create $TAG --repo $REPO_SLUG --verify-tag --title \"\$GH_TITLE\" --notes-file <file> ${ASSETS[*]}" >&2
  echo "   3) 结果未知（网络中断/超时）" >&2
  echo "      → **不要**重跑本脚本；先 git ls-remote --tags origin 与" >&2
  echo "        gh release view $TAG --repo $REPO_SLUG 查清远端实际状态" >&2
  echo >&2
  echo "   原则：同版本不覆盖、不重发。确需重发请 bump Cargo.toml 的 version。" >&2
  exit 1
fi

echo "== 已发布: https://github.com/$REPO_SLUG/releases/tag/$TAG (commit $COMMIT) =="
echo "== 自检: 二进制应报版本 v$VERSION =="
