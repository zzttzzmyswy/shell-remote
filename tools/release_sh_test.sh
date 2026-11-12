#!/bin/bash
# tools/release.sh 的隔离回归测试（MYS-2806 review）。
#
# 全部在临时目录里跑：一个假 git 远端（本地 bare 仓库）、一个假 `gh`、
# 一个假 build-dist 脚本。**不接触真实仓库、真实远端、GitHub**，也不跑真实
# 发布动作。
#
# 覆盖的断言（每条都对应旧版的一个具体缺陷）：
#   1. 缺资产      → 打 tag / 建 release 之前就失败
#   2. 脏工作树    → 打 tag / 建 release 之前就失败（旧版只 WARN）
#   3. 本地已有 tag → 打 tag / 建 release 之前就失败（旧版 `git tag -f` 覆盖）
#   4. 远端已有 tag → 打 tag / 建 release 之前就失败（旧版 `git push -f`）
#   5. 已有 release → 打 tag 之前就失败（旧版先推 tag 再撞 422）
#   6. 正常新版本   → tag 精确指向 HEAD；release 带全部 8 个资产
#   7. 默认会构建  → 调用 build-dist；--skip-build 不调用
#   8. 未知参数 / -h → 报错 / 打印用法（旧版静默忽略一切参数）
#
# 用法: tools/release_sh_test.sh
# 退出码：全部通过 0，任一失败 1。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# RELEASE_SH_UNDER_TEST 可指向任意历史版本，用来确认本测试真的能抓到旧缺陷
# （把 `git show <old>:tools/release.sh` 写进临时文件即可）。
RELEASE_SH="${RELEASE_SH_UNDER_TEST:-$SCRIPT_DIR/release.sh}"
[ -f "$RELEASE_SH" ] || { echo "FAIL: 找不到 $RELEASE_SH"; exit 1; }

WORKROOT="$(mktemp -d "${TMPDIR:-/tmp}/sr-reltest.XXXXXX")"
cleanup() { rm -rf "$WORKROOT"; }
trap cleanup EXIT

# 沙箱里 Cargo.toml 固定 9.9.9 → tag 固定 v9.9.9。
TAG_UNDER_TEST="v9.9.9"

fail=0
ok()  { echo "  ✓ $*"; }
bad() { echo "  ✗ $*"; fail=1; }

# ── 沙箱骨架 ────────────────────────────────────────────────────────
#
# $1 = 沙箱名。建出：
#   <box>/origin   本地 bare 仓库（充当 origin）
#   <box>/repo     工作仓库（含 tools/release.sh 的副本）
#   <box>/bin      假 gh + 假 build-dist 的调用记录
# 每次调用都是全新沙箱，测试之间互不影响。
new_box() {
  local box="$WORKROOT/$1"
  mkdir -p "$box/repo/tools" "$box/repo/dist" "$box/bin"
  git init -q --bare "$box/origin"
  (
    cd "$box/repo" || exit 1
    git init -q .
    git config user.email "test@example.invalid"
    git config user.name "release test"
    git config commit.gpgsign false
    printf '[package]\nname = "shell-remote"\nversion = "9.9.9"\nedition = "2021"\n' > Cargo.toml
    # 与真实仓库一致的忽略规则：dist/ 是构建产物，不该让工作树变"脏"。
    printf 'dist/\n' > .gitignore
    cp "$RELEASE_SH" tools/release.sh
    chmod +x tools/release.sh
    # 假 build-dist 也必须是被跟踪文件，否则默认路径会被脏工作树检查挡下。
    cat > tools/build-dist.sh <<'BD'
#!/bin/bash
echo "build-dist-called" >> "$BUILD_LOG"
cd "$(dirname "$0")/.."
for a in shell-remote-x86_64 shell-remote-aarch64 shell-remote-armv7 shell-remote-x86_64.exe \
         shell-remote-ui-x86_64 shell-remote-ui-aarch64 shell-remote-ui-armv7 shell-remote-ui-x86_64.exe; do
  echo "built" > "dist/$a"
done
BD
    chmod +x tools/build-dist.sh
    git add -A
    git commit -qm "base"
    git remote add origin "$box/origin"
    git push -q origin HEAD:refs/heads/master
  ) || return 1

  # 假 gh：两种记录——
  #   gh.log     每参数一行（`$*` 会把 notes 的换行压平，看不出参数边界）
  #   calls.log  结构化调用标记（`CALL release-view` / `CALL release-create`），
  #              断言必须读它；在 gh.log 里 grep "release create" 永远不匹配。
  # 另：`release create` 可模拟三种结局（见下方环境变量）。
  cat > "$box/bin/gh" <<'GH'
#!/bin/bash
{
  for a in "$@"; do printf 'ARG %s\n' "$a"; done
} >> "$GH_LOG"
if [ "$1" = "release" ] && [ "$2" = "view" ]; then
  echo "CALL release-view" >> "$CALLS_LOG"
  # GH_VIEW_ERROR=1：网络/认证失败（**不是** 404），不能当作"不存在"
  if [ "${GH_VIEW_ERROR:-0}" = "1" ]; then
    echo "error: HTTP 401: Bad credentials (https://api.github.com/graphql)" >&2
    exit 1
  fi
  [ "${GH_EXISTING_RELEASE:-0}" = "1" ] && exit 0
  echo "release not found" >&2
  exit 1
fi
if [ "$1" = "release" ] && [ "$2" = "create" ]; then
  echo "CALL release-create" >> "$CALLS_LOG"
  # GH_FAIL_CREATE=1：创建/上传/清理失败，但 **release 已创建**（review 探针场景）
  if [ "${GH_FAIL_CREATE:-0}" = "1" ]; then
    echo "release created, but asset upload failed: HTTP 502" >&2
    exit 1
  fi
  # GH_UNKNOWN_REMOTE=1：结果未知（网络中断，无法判断远端是否已创建）
  if [ "${GH_UNKNOWN_REMOTE:-0}" = "1" ]; then
    echo "error: Post \"https://api.github.com/...\": dial tcp: i/o timeout" >&2
    exit 1
  fi
  exit 0
fi
exit 0
GH
  chmod +x "$box/bin/gh"
  echo "$box"
}

# 补齐 8 个资产（不经过 build-dist）。
seed_assets() {
  local repo="$1"
  for a in shell-remote-x86_64 shell-remote-aarch64 shell-remote-armv7 shell-remote-x86_64.exe \
           shell-remote-ui-x86_64 shell-remote-ui-aarch64 shell-remote-ui-armv7 shell-remote-ui-x86_64.exe; do
    echo "asset" > "$repo/dist/$a"
  done
}

# 在沙箱里跑 release.sh，返回其退出码；输出写进 $box/out.log。
run_release() { # $1 = box, rest = args
  local box="$1"; shift
  (
    cd "$box/repo" || exit 1
    PATH="$box/bin:$PATH" \
      GH_LOG="$box/gh.log" CALLS_LOG="$box/calls.log" BUILD_LOG="$box/build.log" \
      GH_EXISTING_RELEASE="${GH_EXISTING_RELEASE:-0}" \
      GH_FAIL_CREATE="${GH_FAIL_CREATE:-0}" \
      GH_VIEW_ERROR="${GH_VIEW_ERROR:-0}" \
      GH_UNKNOWN_REMOTE="${GH_UNKNOWN_REMOTE:-0}" \
      bash tools/release.sh "$@"
  ) > "$box/out.log" 2>&1
  echo $?
}

# 该沙箱是否**没有**发生任何破坏性动作（无 tag、无 release 调用）。
#
# 判据读 `calls.log` 里的结构化调用标记，而不是在 `gh.log` 里 grep
# "release create"——后者永远不匹配（gh.log 是逐行 `ARG ...` 记录），
# 旧版这个断言实际上是空的（MYS-2806 review 3）。
no_side_effects() { # $1 = box
  local box="$1"
  if git -C "$box/repo" rev-parse -q --verify "refs/tags/$TAG_UNDER_TEST" >/dev/null; then
    return 1
  fi
  if git -C "$box/origin" rev-parse -q --verify "refs/tags/$TAG_UNDER_TEST" >/dev/null; then
    return 1
  fi
  if [ -f "$box/calls.log" ] && grep -qx "CALL release-create" "$box/calls.log"; then
    return 1
  fi
  return 0
}

# 是否调用过 `gh release create`（结构化标记，可靠）。
release_create_called() { # $1 = box
  [ -f "$1/calls.log" ] && grep -qx "CALL release-create" "$1/calls.log"
}

echo "── release.sh 隔离测试（$WORKROOT）──"

# ── 1. 缺资产 ──────────────────────────────────────────────────────
{
  box="$(new_box missing-assets)"
  # 只放一半资产：旧版只检查两个文件就继续。
  echo x > "$box/repo/dist/shell-remote-x86_64"
  echo x > "$box/repo/dist/shell-remote-ui-x86_64"
  rc="$(run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "1. 缺资产：非 0 退出（$rc）" || bad "1. 缺资产却成功退出"
  no_side_effects "$box" && ok "1. 缺资产：未打 tag、未建 release" \
    || bad "1. 缺资产：发生了破坏性动作"
  grep -q "缺少制品" "$box/out.log" && ok "1. 缺资产：报出缺哪个文件" \
    || bad "1. 缺资产：错误信息没指出缺失文件"
}

# ── 2. 脏工作树 ────────────────────────────────────────────────────
{
  box="$(new_box dirty-tree)"
  seed_assets "$box/repo"
  echo "uncommitted" >> "$box/repo/Cargo.toml"   # dist/ 被 .gitignore 忽略，改已跟踪文件才脏
  rc="$(run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "2. 脏工作树：非 0 退出（$rc）" || bad "2. 脏工作树却成功退出"
  no_side_effects "$box" && ok "2. 脏工作树：未打 tag、未建 release" \
    || bad "2. 脏工作树：发生了破坏性动作"
  grep -q "拒绝发布" "$box/out.log" && ok "2. 脏工作树：给出拒绝原因" \
    || bad "2. 脏工作树：没有明确拒绝说明"
}

# ── 3. 本地已有 tag ────────────────────────────────────────────────
{
  box="$(new_box local-tag)"
  seed_assets "$box/repo"
  ( cd "$box/repo" && git tag v9.9.9 )
  before="$(git -C "$box/repo" rev-parse v9.9.9)"
  ( cd "$box/repo" && git commit -q --allow-empty -m ahead )
  rc="$(run_release "$box" --skip-build)"
  after="$(git -C "$box/repo" rev-parse v9.9.9)"
  [ "$rc" -ne 0 ] && ok "3. 本地已有 tag：非 0 退出（$rc）" || bad "3. 本地已有 tag 却成功退出"
  [ "$before" = "$after" ] && ok "3. 本地已有 tag：未被覆盖（无 tag -f）" \
    || bad "3. 本地已有 tag：被强制移动了"
  release_create_called "$box" && bad "3. 本地已有 tag：仍建了 release" \
    || ok "3. 本地已有 tag：未建 release（读结构化调用标记）"
}

# ── 4. 远端已有 tag ────────────────────────────────────────────────
{
  box="$(new_box remote-tag)"
  seed_assets "$box/repo"
  # 在远端造一个同名 tag（本地不建）。
  ( cd "$box/repo" && git push -q origin HEAD:refs/tags/v9.9.9 )
  remote_before="$(git -C "$box/origin" rev-parse v9.9.9)"
  ( cd "$box/repo" && git commit -q --allow-empty -m ahead )
  rc="$(run_release "$box" --skip-build)"
  remote_after="$(git -C "$box/origin" rev-parse v9.9.9)"
  [ "$rc" -ne 0 ] && ok "4. 远端已有 tag：非 0 退出（$rc）" || bad "4. 远端已有 tag 却成功退出"
  [ "$remote_before" = "$remote_after" ] && ok "4. 远端已有 tag：未被强推覆盖" \
    || bad "4. 远端已有 tag：被 push -f 覆盖了"
  release_create_called "$box" && bad "4. 远端已有 tag：仍建了 release" \
    || ok "4. 远端已有 tag：未建 release（读结构化调用标记）"
}

# ── 5. 已有 release ────────────────────────────────────────────────
{
  box="$(new_box existing-release)"
  seed_assets "$box/repo"
  rc="$(GH_EXISTING_RELEASE=1 run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "5. 已有 release：非 0 退出（$rc）" || bad "5. 已有 release 却成功退出"
  no_side_effects "$box" && ok "5. 已有 release：打 tag 之前就失败" \
    || bad "5. 已有 release：tag 已经被推出去了才失败"
}

# ── 6. 正常新版本：tag 指向确切 commit + 8 个资产 ─────────────────
{
  box="$(new_box happy-path)"
  seed_assets "$box/repo"
  head="$(git -C "$box/repo" rev-parse HEAD)"
  rc="$(run_release "$box" --skip-build)"
  [ "$rc" -eq 0 ] && ok "6. 正常发布：退出 0" || { bad "6. 正常发布：退出 $rc"; sed -n '1,20p' "$box/out.log"; }
  # tag 是 annotated（带 release 消息），所以要 `^{commit}` 解析到提交。
  tag_commit="$(git -C "$box/repo" rev-parse -q --verify "v9.9.9^{commit}" 2>/dev/null)"
  [ "$tag_commit" = "$head" ] && ok "6. tag 精确指向 HEAD（$head）" \
    || bad "6. tag 指向 $tag_commit，应为 $head"
  remote_commit="$(git -C "$box/origin" rev-parse -q --verify "v9.9.9^{commit}" 2>/dev/null)"
  [ "$remote_commit" = "$head" ] && ok "6. 远端 tag 指向同一 commit" \
    || bad "6. 远端 tag 指向 ${remote_commit:-<空>}"
  # 每个资产必须是**独立参数**（`ARG ./dist/...`），不能被续行拼进 notes。
  assets_in_call="$(grep -c '^ARG .*dist/shell-remote' "$box/gh.log")"
  [ "$assets_in_call" -eq 8 ] && ok "6. release 带 8 个资产（各自独立参数）" \
    || bad "6. release 只带了 $assets_in_call 个独立资产参数"
  grep -q -- '^ARG --verify-tag$' "$box/gh.log" && ok "6. release 校验已推送的 tag" \
    || bad "6. release 没有 --verify-tag"
  # notes 参数本身不得含 dist/ 路径。
  # 说明（MYS-2806 review 3）：原脚本**并没有**这个缺陷——heredoc 在 `$( )` 里
  # 是正确的。旧版测试之所以"通过"，是因为它用 `echo "gh $*"` 把多行 notes
  # 压成一行后看到了后面的资产路径，属于测量假象。这条断言保留，用来防止
  # 将来真的把资产续行拼进 notes；它是防回归，不是修复证据。
  notes_arg="$(awk '/^ARG --notes$/{getline; print; exit}' "$box/gh.log")"
  printf '%s' "$notes_arg" | grep -q 'dist/' && bad "6. notes 参数里出现了资产路径" \
    || ok "6. notes 参数不含资产路径"
}

# ── 7. 默认构建 vs --skip-build ──────────────────────────────────
{
  box="$(new_box default-builds)"
  # 不给资产，让默认路径必须调用 build-dist.sh 才能成功。
  rc="$(run_release "$box")"
  [ "$rc" -eq 0 ] && ok "7. 默认路径：退出 0" || { bad "7. 默认路径：退出 $rc"; sed -n '1,20p' "$box/out.log"; }
  grep -q "build-dist-called" "$box/build.log" 2>/dev/null \
    && ok "7. 默认路径：调用了 build-dist.sh" \
    || bad "7. 默认路径：从未调用 build-dist.sh（旧版声称构建却什么都没做）"
}
{
  box="$(new_box skip-build)"
  seed_assets "$box/repo"
  rc="$(run_release "$box" --skip-build)"
  [ "$rc" -eq 0 ] && ok "7. --skip-build：退出 0" || bad "7. --skip-build：退出 $rc"
  [ -f "$box/build.log" ] && bad "7. --skip-build：仍然调用了 build-dist.sh" \
    || ok "7. --skip-build：未调用 build-dist.sh"
}

# ── 8. 未知参数 / -h ─────────────────────────────────────────────
{
  box="$(new_box unknown-arg)"
  seed_assets "$box/repo"
  rc="$(run_release "$box" --no-such-flag)"
  [ "$rc" -eq 2 ] && ok "8. 未知参数：退出 2" || bad "8. 未知参数：退出 $rc（应拒绝而非忽略）"
  no_side_effects "$box" && ok "8. 未知参数：未发生副作用" || bad "8. 未知参数：发生了副作用"
}
{
  box="$(new_box help)"
  rc="$(run_release "$box" -h)"
  [ "$rc" -eq 0 ] && ok "8. -h：退出 0" || bad "8. -h：退出 $rc"
  grep -q -- "--skip-build" "$box/out.log" && ok "8. -h：输出里含 --skip-build（与帮助一致）" \
    || bad "8. -h：帮助文本没提到 --skip-build"
  no_side_effects "$box" && ok "8. -h：未发生副作用" || bad "8. -h：发生了副作用"
}

# ── 9. release 已创建但上传/清理失败 → 必须**保留** tag，报告状态 ──
#
# MYS-2806 review 2 的确定性场景：`gh release create` 内部是「创建 release →
# 上传资产 → 发布 → 清理草稿」多个请求。失败可能发生在创建**之后**，此时远端
# 已有 release。盲目删 tag 会留下"release 存在、tag 没了"的更糟状态。
{
  box="$(new_box gh-partial)"
  seed_assets "$box/repo"
  rc="$(GH_FAIL_CREATE=1 run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "9. 创建后上传失败：非 0 退出（$rc）" || bad "9. 创建后上传失败却退出 0"
  # release create 确实被调用（结构化标记）
  release_create_called "$box" && ok "9. 确实调用了 gh release create" \
    || bad "9. 没有调用 gh release create"
  # 关键：本地与远端 tag 都必须**保留**
  if git -C "$box/repo" rev-parse -q --verify "refs/tags/$TAG_UNDER_TEST" >/dev/null; then
    ok "9. 本地 tag 保留（未被盲目删除）"
  else
    bad "9. 本地 tag 被删除了（release 可能已创建，删 tag 会更糟）"
  fi
  if git -C "$box/origin" rev-parse -q --verify "refs/tags/$TAG_UNDER_TEST" >/dev/null; then
    ok "9. 远端 tag 保留（未被盲目删除）"
  else
    bad "9. 远端 tag 被删除了 —— review 探针场景：release_created=true 但 remote_tag_exists=false"
  fi
  # 必须报告实际状态与恢复方法，而不是只说"失败了"
  grep -q "已保留" "$box/out.log" && ok "9. 明确报告 tag 已保留" \
    || bad "9. 没有说明 tag 的处理结果"
  grep -q "gh release view" "$box/out.log" && ok "9. 给出状态确认命令" \
    || bad "9. 没给出如何确认远端状态"
  grep -q "gh release upload" "$box/out.log" && ok "9. 给出补传资产的恢复命令" \
    || bad "9. 没给出恢复方法"
  grep -qi "不要.*重跑\|不覆盖\|不重发" "$box/out.log" && ok "9. 明确禁止盲目重跑" \
    || bad "9. 没警告不要重跑"
}

# ── 10. 远端结果未知（网络中断）→ 保留 tag 并报告，不猜 ──────────
{
  box="$(new_box gh-unknown)"
  seed_assets "$box/repo"
  rc="$(GH_UNKNOWN_REMOTE=1 run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "10. 结果未知：非 0 退出（$rc）" || bad "10. 结果未知却退出 0"
  if git -C "$box/origin" rev-parse -q --verify "refs/tags/$TAG_UNDER_TEST" >/dev/null; then
    ok "10. 结果未知时远端 tag 保留"
  else
    bad "10. 结果未知时远端 tag 被删除（无法确认远端状态时不得删除引用）"
  fi
  grep -qi "结果未知\|未知" "$box/out.log" && ok "10. 明确标出结果未知" \
    || bad "10. 没有区分"结果未知"与"确实失败""
}

# ── 11. 远端 tag / release 查询失败 ≠ 不存在 ─────────────────────
{
  # 11a：ls-remote 失败（远端不可达）→ 必须在发布动作前退出
  box="$(new_box query-fail-tag)"
  seed_assets "$box/repo"
  # 让 origin 指向不存在的路径，ls-remote 会失败
  ( cd "$box/repo" && git remote set-url origin "$box/does-not-exist.git" )
  rc="$(run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "11a. 远端查询失败：非 0 退出（$rc）" || bad "11a. 远端查询失败却继续发布"
  release_create_called "$box" && bad "11a. 查询失败后仍然创建了 release" \
    || ok "11a. 查询失败时未创建 release"
  grep -q "查询失败\|无法查询" "$box/out.log" && ok "11a. 明确区分查询失败与不存在" \
    || bad "11a. 把查询失败当成"远端不存在""
}
{
  # 11b：gh release view 因网络/认证失败（非 404）→ 必须停止，不能当不存在
  box="$(new_box query-fail-gh)"
  seed_assets "$box/repo"
  rc="$(GH_VIEW_ERROR=1 run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "11b. gh 查询失败：非 0 退出（$rc）" || bad "11b. gh 查询失败却继续发布"
  release_create_called "$box" && bad "11b. 查询失败后仍然创建了 release" \
    || ok "11b. 查询失败时未创建 release"
  grep -q "查询失败\|无法确认" "$box/out.log" && ok "11b. 明确区分查询失败与确实不存在" \
    || bad "11b. 把 gh 查询失败当成"release 不存在""
}

# ── 12. 元测试：证明「是否调用 gh release create」这类断言真的有效 ──
#
# review 指出旧版用 `grep "release create" gh.log` 检查，而 mock 记的是逐行
# `ARG release` / `ARG create`，永远不匹配 —— 那个断言实际上是空的。这里用
# **已知会调用** 与 **已知不会调用** 两个沙箱反过来验证 `release_create_called`
# 判据本身，确保它不是又一个恒真/恒假的空断言。
{
  box="$(new_box meta-called)"
  seed_assets "$box/repo"
  ( cd "$box/repo" && git commit -q --allow-empty -m x )   # 保证能正常发布
  run_release "$box" --skip-build >/dev/null
  release_create_called "$box" && ok "12. 元测试：确实调用时判据为真（断言非恒假）" \
    || bad "12. 元测试：实际调用了 gh release create，判据却为假 → 断言失效"

  box2="$(new_box meta-not-called)"
  seed_assets "$box2/repo"
  ( cd "$box2/repo" && git tag "$TAG_UNDER_TEST" )   # 已有 tag → 不会调用
  run_release "$box2" --skip-build >/dev/null
  release_create_called "$box2" && bad "12. 元测试：未调用时判据为真 → 断言失效" \
    || ok "12. 元测试：未调用时判据为假（断言非恒真）"

  # 再证「旧写法」确实无效：gh.log 里没有 "release create" 这一行
  if grep -q "release create" "$box/gh.log" 2>/dev/null; then
    bad "12. 元测试：gh.log 竟然包含 'release create'，说明探针假设有误"
  else
    ok "12. 元测试：旧写法 grep 'release create' 确实永不匹配（已废弃）"
  fi
}

echo
if [ "$fail" -eq 0 ]; then echo "PASS"; else echo "FAIL"; fi
exit "$fail"
