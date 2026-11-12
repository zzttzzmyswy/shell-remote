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

  # 假 gh：记录每次调用（**每个参数一行**：`$*` 会把 notes 的换行压平，
  # 就看不出资产到底是被当成独立参数还是拼进了 notes）；`release view`
  # 默认"不存在"（退出 1）。
  cat > "$box/bin/gh" <<'GH'
#!/bin/bash
{
  echo "=== gh call ==="
  for a in "$@"; do printf 'ARG %s\n' "$a"; done
} >> "$GH_LOG"
if [ "$1" = "release" ] && [ "$2" = "view" ]; then
  # GH_EXISTING_RELEASE=1 → 假装该 release 已存在
  [ "${GH_EXISTING_RELEASE:-0}" = "1" ] && exit 0
  exit 1
fi
if [ "$1" = "release" ] && [ "$2" = "create" ]; then
  [ "${GH_FAIL_CREATE:-0}" = "1" ] && { echo "HTTP 422: already exists" >&2; exit 1; }
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
      GH_LOG="$box/gh.log" BUILD_LOG="$box/build.log" \
      GH_EXISTING_RELEASE="${GH_EXISTING_RELEASE:-0}" \
      GH_FAIL_CREATE="${GH_FAIL_CREATE:-0}" \
      bash tools/release.sh "$@"
  ) > "$box/out.log" 2>&1
  echo $?
}

# 该沙箱是否**没有**发生任何破坏性动作（无 tag、无 release 调用）。
no_side_effects() { # $1 = box
  local box="$1"
  if git -C "$box/repo" rev-parse -q --verify "refs/tags/v9.9.9" >/dev/null; then
    return 1
  fi
  if git -C "$box/origin" rev-parse -q --verify "refs/tags/v9.9.9" >/dev/null; then
    return 1
  fi
  if [ -f "$box/gh.log" ] && grep -q "release create" "$box/gh.log"; then
    return 1
  fi
  return 0
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
  grep -q "release create" "$box/gh.log" 2>/dev/null && bad "3. 本地已有 tag：仍建了 release" \
    || ok "3. 本地已有 tag：未建 release"
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
  grep -q "release create" "$box/gh.log" 2>/dev/null && bad "4. 远端已有 tag：仍建了 release" \
    || ok "4. 远端已有 tag：未建 release"
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
  # notes 里不该出现 dist/ 路径。
  notes_arg="$(awk '/^ARG ## v9.9.9/{f=1} f&&/^ARG /{print} f&&/shell-remote-ui-\*/{exit}' "$box/gh.log")"
  echo "$notes_arg" | grep -q 'dist/' && bad "6. notes 里混进了资产路径（旧版缺陷）" \
    || ok "6. notes 内容干净（不含资产路径）"
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

# ── 9. 发布失败要回滚本地 tag ────────────────────────────────────
{
  box="$(new_box gh-fails)"
  seed_assets "$box/repo"
  rc="$(GH_FAIL_CREATE=1 run_release "$box" --skip-build)"
  [ "$rc" -ne 0 ] && ok "9. release 失败：非 0 退出（$rc）" || bad "9. release 失败却退出 0"
  if git -C "$box/repo" rev-parse -q --verify "refs/tags/v9.9.9" >/dev/null; then
    bad "9. release 失败后本地 tag 仍然存在（下次重跑会撞"已存在 tag"）"
  else
    ok "9. release 失败后本地 tag 已回滚"
  fi
}

echo
if [ "$fail" -eq 0 ]; then echo "PASS"; else echo "FAIL"; fi
exit "$fail"
