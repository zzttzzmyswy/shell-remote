#!/bin/bash
# 低空间构建配置的合同测试（MYS-2836 阶段1）。
#
# 覆盖的断言（每条对应一个具体的失败模式）：
#   P1  [profile.dev]/[profile.test] 收紧：debug≠2、incremental=false
#   P2  依赖（package."*"）debug=false —— 这是体积降幅的主要来源
#   P3  完整调试 profile 存在且 debug=2（含依赖）—— 「需要时显式开启」不留死路
#   P4  [profile.release] 未被改动 —— 发布产物契约不变（opt-level=z/lto/strip/panic）
#   P5  Cargo.toml 能被 cargo 解析（profile 名/继承语法合法）
#   P6  test_lib.sh 语法正确（bash -n）且可执行
#   P7  test_lib.sh 不覆盖 CARGO_TARGET_DIR；默认值就是 cargo 的默认 <repo>/target
#   P8  test_lib.sh 把 CARGO_TARGET_DIR 原样传给 cargo（与取件路径一致）
#   P9  test_lib.sh 失败正确传播（cargo 非零 → 脚本非零）
#   P10 test_lib.sh 空间不足时开工前就失败，不进入编译
#   P11 profile 收紧确实传到了 rustc（真实编译探针，不是只读 toml）
#   P12 --full-debug 确实把 debug 还原成 2（真实编译探针）
#   P13 --incremental 确实打开了 -C incremental（真实编译探针）
#   P14 第二次运行同一命令有真实复用（cargo 不重编）
#   P15 文档记录了低空间测试与完整调试的恢复命令
#
# 全部在临时沙箱里跑，用假 cargo 探测参数；只有 P11–P13 用一个极小 crate
# 做真实 rustc 探针（秒级），不编译本仓库、不碰真实 target。
#
# 用法: tools/build_space_contract_test.sh
# 退出码：全部通过 0，任一失败 1。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_LIB="$SCRIPT_DIR/test_lib.sh"
CARGO_TOML="$ROOT/Cargo.toml"
BUILD_MD="$ROOT/docs/build.md"

WORKROOT="$(mktemp -d "${TMPDIR:-/tmp}/sr-space.XXXXXX")"
cleanup() { [ -n "${KEEP_WORKROOT:-}" ] || rm -rf "$WORKROOT"; }
trap cleanup EXIT

fail=0
total=0
ok()  { total=$((total+1)); echo "  ✓ $*"; }
bad() { total=$((total+1)); fail=1; echo "  ✗ $*"; }

echo "── P1–P5 Cargo.toml profile 契约 ──"

# 用 python 解析 toml，避免 grep 被注释/嵌套块骗到。
py() { python3 - "$@"; }

PROF_JSON="$WORKROOT/profiles.json"
py "$CARGO_TOML" > "$PROF_JSON" <<'PY'
import sys, json
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], "rb") as f:
    doc = tomllib.load(f)
prof = doc.get("profile", {})
out = {}
for name, body in prof.items():
    if not isinstance(body, dict):
        continue
    flat = {}
    for k, v in body.items():
        if k == "package" and isinstance(v, dict):
            for pkg, pbody in v.items():
                if isinstance(pbody, dict):
                    flat["pkg:" + pkg] = pbody
        elif not isinstance(v, dict):
            flat[k] = v
    out[name] = flat
print(json.dumps(out))
PY
[ -s "$PROF_JSON" ] && ok "P5 Cargo.toml 可被解析" || bad "P5 Cargo.toml 解析失败"

jget() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); v=d.get(sys.argv[2],{}).get(sys.argv[3],"<absent>"); print(json.dumps(v))' "$PROF_JSON" "$1" "$2"; }
jget_pkg() { # $1=profile $2=package-glob $3=key
  python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); v=d.get(sys.argv[2],{}).get("pkg:"+sys.argv[3],{}).get(sys.argv[4],"<absent>"); print(json.dumps(v))' "$PROF_JSON" "$1" "$2" "$3"
}

# P1: dev/test 收紧
for p in dev test; do
  dbg=$(jget "$p" debug)
  inc=$(jget "$p" incremental)
  case "$dbg" in
    '"line-tables-only"'|'"limited"'|'false'|'"0"') ok "P1 $p.debug=$dbg（非完整 DWARF）" ;;
    '<absent>') bad "P1 $p.debug 未设置（仍是默认 debug=2）" ;;
    *) bad "P1 $p.debug=$dbg 不是预期值" ;;
  esac
  [ "$inc" = "false" ] && ok "P1 $p.incremental=false" || bad "P1 $p.incremental=$inc（期望 false）"
done

# P2: 依赖 debug=false
for p in dev test; do
  dbg=$(jget_pkg "$p" '*' debug)
  [ "$dbg" = "false" ] && ok "P2 $p.package.\"*\".debug=false" || bad "P2 $p.package.\"*\" debug=$dbg（期望 false）"
done

# P3: 完整调试 profile
for p in dev-full test-full; do
  inh=$(jget "$p" inherits)
  dbg=$(jget "$p" debug)
  [ "$inh" != '"<absent>"' ] && ok "P3 $p 继承 $inh" || bad "P3 $p 缺少 inherits"
  [ "$dbg" = "2" ] && ok "P3 $p.debug=2（完整调试可恢复）" || bad "P3 $p.debug=$dbg（期望 2）"
  pdbg=$(jget_pkg "$p" '*' debug)
  [ "$pdbg" = "2" ] && ok "P3 $p.package.\"*\".debug=2（依赖也可单步）" || bad "P3 $p 依赖 debug=$pdbg（期望 2）"
done

# P4: release 契约不变
rl=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps(d.get("release",{}),sort_keys=True))' "$PROF_JSON")
for kv in '"opt-level": "z"' '"lto": true' '"codegen-units": 1' '"strip": true' '"panic": "unwind"'; do
  echo "$rl" | grep -q "$kv" && ok "P4 release $kv 未变" || bad "P4 release 缺少 $kv（发布契约被改动）"
done

echo
echo "── P6–P10 test_lib.sh 契约 ──"

[ -x "$TEST_LIB" ] && ok "P6 test_lib.sh 可执行" || bad "P6 test_lib.sh 不可执行"
bash -n "$TEST_LIB" 2>"$WORKROOT/syn.err" && ok "P6 bash -n 通过" \
  || { bad "P6 bash -n 失败"; sed -n '1,5p' "$WORKROOT/syn.err"; }

# 假 cargo：把收到的参数与关键环境变量记下来，退出码可控。
FAKEBIN="$WORKROOT/fakebin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/cargo" <<'FAKE'
#!/bin/bash
{
  echo "ARGV: $*"
  echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset>}"
  echo "CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-<unset>}"
  echo "PWD=$PWD"
} > "${FAKE_CARGO_LOG:?}"
[ -n "${FAKE_CARGO_TOUCH:-}" ] && mkdir -p "$(dirname "$FAKE_CARGO_TOUCH")" && : > "$FAKE_CARGO_TOUCH"
exit "${FAKE_CARGO_RC:-0}"
FAKE
chmod +x "$FAKEBIN/cargo"

run_with_fake() { # $1=outdir, rest = args to test_lib
  local out="$1"; shift
  rm -f "$out.log"
  FAKE_CARGO_LOG="$out.log" PATH="$FAKEBIN:$PATH" \
    SR_MIN_FREE_MB=0 CARGO_TARGET_DIR="${FAKE_TARGET_DIR:-}" \
    bash "$TEST_LIB" "$@" >"$out.out" 2>&1
  echo $?
}

# P7: 默认 CARGO_TARGET_DIR 是 <repo>/target，且脚本不自己改写它
rc=$(run_with_fake "$WORKROOT/p7" --no-run)
grep -q "^CARGO_TARGET_DIR=$ROOT/target$" "$WORKROOT/p7.log" \
  && ok "P7 默认 target 就是 <repo>/target（cargo 默认值）" \
  || { bad "P7 默认 target 不是 <repo>/target"; grep '^CARGO_TARGET_DIR=' "$WORKROOT/p7.log"; }
grep -q '^PWD='"$ROOT"'$' "$WORKROOT/p7.log" && ok "P7 在仓库根目录运行" || bad "P7 没在仓库根跑"
grep -q '^ARGV: test --lib' "$WORKROOT/p7.log" && ok "P7 走 cargo test --lib" || bad "P7 没用 cargo test --lib"

# P8: 外部 CARGO_TARGET_DIR 原样透传（取件路径一致的前提）
EXT="$WORKROOT/ext-target"
rc=$(FAKE_TARGET_DIR="$EXT" run_with_fake "$WORKROOT/p8" --no-run)
grep -q "^CARGO_TARGET_DIR=$EXT$" "$WORKROOT/p8.log" \
  && ok "P8 外部 CARGO_TARGET_DIR 原样传给 cargo" \
  || { bad "P8 CARGO_TARGET_DIR 未被透传"; grep '^CARGO_TARGET_DIR=' "$WORKROOT/p8.log"; }

# P9: 失败传播
rc=$(FAKE_CARGO_RC=7 FAKE_CARGO_LOG="$WORKROOT/p9.log" PATH="$FAKEBIN:$PATH" SR_MIN_FREE_MB=0 \
  CARGO_TARGET_DIR="" bash "$TEST_LIB" --no-run >/dev/null 2>&1; echo $?)
[ "$rc" = "7" ] && ok "P9 cargo 失败码 7 原样传播" || bad "P9 失败码是 $rc（期望 7）"

# P10: 空间不足开工前就失败，且不调用 cargo
rc=$(FAKE_CARGO_LOG="$WORKROOT/p10.log" PATH="$FAKEBIN:$PATH" SR_MIN_FREE_MB=999999999 \
  CARGO_TARGET_DIR="" bash "$TEST_LIB" --no-run >"$WORKROOT/p10.out" 2>&1; echo $?)
[ "$rc" -ne 0 ] && ok "P10 空间不足 → 非零退出（$rc）" || bad "P10 空间不足仍返回 0"
[ -f "$WORKROOT/p10.log" ] && bad "P10 空间不足却仍调用了 cargo" || ok "P10 未进入编译"

echo
echo "── P11–P13 真实 rustc 探针（极小 crate，秒级）──"

# 探针 crate：不依赖任何第三方，避免联网。
PROBE="$WORKROOT/probe"
mkdir -p "$PROBE/src"
cat > "$PROBE/Cargo.toml" <<'TOMLEOF'
[package]
name = "spaceprobe"
version = "0.1.0"
edition = "2021"
TOMLEOF
echo 'pub fn f() -> u32 { 1 }' > "$PROBE/src/lib.rs"

# 用本仓库的 profile 段落做探针（只取 profile，不取依赖），这样验的是
# 真正会生效的配置，而不是另写一份。
# 生成探针用 profile：把本仓库的 dev/test 数值原样抄进去（数值取自解析结果）。
gen_profile() {
  python3 - "$CARGO_TOML" <<'PY'
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], "rb") as f:
    prof = tomllib.load(f).get("profile", {})

def fmt(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, str):
        return '"%s"' % v
    return str(v)

lines = []
for name, body in prof.items():
    if not isinstance(body, dict):
        continue
    lines.append("[profile.%s]" % name)
    pkg = {}
    for k, v in body.items():
        if k == "package" and isinstance(v, dict):
            pkg = v
            continue
        if isinstance(v, dict):
            continue
        lines.append("%s = %s" % (k, fmt(v)))
    for glob, sbody in pkg.items():
        if not isinstance(sbody, dict):
            continue
        lines.append('[profile.%s.package."%s"]' % (name, glob))
        for k, v in sbody.items():
            lines.append("%s = %s" % (k, fmt(v)))
    lines.append("")
print("\n".join(lines))
PY
}
gen_profile >> "$PROBE/Cargo.toml"

# 抓真实 rustc 命令行里的 debuginfo / incremental。
probe_flags() { # $1 = 额外 cargo 参数
  local td="$WORKROOT/pt-$$-$RANDOM"
  ( cd "$PROBE" && CARGO_TARGET_DIR="$td" cargo test --lib --no-run -v "$@" 2>&1 ) \
    | grep -E "Running .*rustc --crate-name spaceprobe " | tail -1
}

line=$(probe_flags "")
echo "$line" | grep -q -- '-C debuginfo=line-tables-only' \
  && ok "P11 默认 profile 实际传 debuginfo=line-tables-only" \
  || { bad "P11 默认 profile 的 debuginfo 不是 line-tables-only"; echo "    ${line:0:200}"; }
echo "$line" | grep -q -- '-C incremental=' \
  && bad "P11 默认 profile 仍开了 incremental" \
  || ok "P11 默认 profile 没有 -C incremental（增量关闭）"

line=$(probe_flags "--profile" "test-full")
echo "$line" | grep -q -- '-C debuginfo=2' \
  && ok "P12 --profile test-full 传 debuginfo=2" \
  || { bad "P12 test-full 的 debuginfo 不是 2"; echo "    ${line:0:200}"; }

line=$( cd "$PROBE" && CARGO_INCREMENTAL=1 CARGO_TARGET_DIR="$WORKROOT/pt-incr" cargo test --lib --no-run -v 2>&1 | grep -E "Running .*rustc --crate-name spaceprobe " | tail -1 )
echo "$line" | grep -q -- '-C incremental=' \
  && ok "P13 CARGO_INCREMENTAL=1 能重新打开增量" \
  || { bad "P13 CARGO_INCREMENTAL=1 没生效"; echo "    ${line:0:200}"; }

echo
echo "── P14 第二次运行有真实复用 ──"
RT="$WORKROOT/reuse"
( cd "$PROBE" && CARGO_TARGET_DIR="$RT" cargo test --lib --no-run >"$WORKROOT/r1.log" 2>&1 )
sz1=$(du -sk "$RT" 2>/dev/null | awk '{print $1}')
( cd "$PROBE" && CARGO_TARGET_DIR="$RT" cargo test --lib --no-run >"$WORKROOT/r2.log" 2>&1 )
sz2=$(du -sk "$RT" 2>/dev/null | awk '{print $1}')
if grep -q "Compiling spaceprobe" "$WORKROOT/r2.log"; then
  bad "P14 第二次仍重编（没有复用）"
else
  ok "P14 第二次运行未重编（cargo 指纹复用）"
fi
[ -n "$sz1" ] && [ "$sz1" = "$sz2" ] && ok "P14 复用后 target 体积不增长（${sz1}KB）" \
  || bad "P14 target 体积异常：$sz1 → $sz2"

echo
echo "── P15 文档 ──"
[ -f "$BUILD_MD" ] && ok "P15 docs/build.md 存在" || bad "P15 缺少 docs/build.md"
grep -q "test_lib.sh" "$BUILD_MD" && ok "P15 文档记录了低空间测试入口" || bad "P15 文档未提及 test_lib.sh"
grep -q "test-full" "$BUILD_MD" && ok "P15 文档记录了完整调试的恢复命令" || bad "P15 文档未提及 test-full"

echo
if [ "$fail" -eq 0 ]; then
  echo "PASS（$total 项断言）"
  exit 0
else
  echo "FAIL（$total 项断言中有失败）"
  exit 1
fi
