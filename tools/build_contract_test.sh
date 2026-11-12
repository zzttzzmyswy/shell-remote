#!/bin/bash
# tools/build-dist.sh / tools/build-gnu-ui.sh 的构建合同测试（MYS-2828）。
#
# 全部在临时目录里跑：假的 cargo / 交叉工具链 / docker，以及用真实
# readelf / strings / file 探测的合成 ELF / PE 制品。**不编译任何真实代码、
# 不联网、不碰真实 target 与 dist**。
#
# 覆盖的断言（每条对应一个具体的失败模式）：
#   A1  正常路径：八件齐全、逐字节落到 dist/、导出 BUILD-INFO.txt
#   A2  缺目标文件            → 非零，dist 完全不被改动
#   A3  版本串不符            → 非零
#   A4  架构不符              → 非零
#   A5  Linux UI 是 musl 静态  → 非零（旧版末尾只查 AV1，静态 UI 能混过去）
#   A6  Linux UI 缺 AV1        → 非零
#   A7  Windows UI 缺 AV1      → 非零（旧版 AV1 门禁只查 Linux 三件）
#   A8  Windows UI subsystem 错 → 非零
#   A9  Windows UI 缺 admin manifest → 非零
#   A10 Windows libaom 缺失    → 开工前就失败，且不进入任何编译
#   A11 CARGO_TARGET_DIR 覆盖  → 取的是新目录里的文件（不是旧的 $ROOT/target）
#   A12 cargo 写到了别处       → 非零，不静默拿旧文件顶上
#   A13 staging 中途复制失败    → 非零，dist 保持上一版（不留新旧混合）
#   A14 lean CLI 里出现 AV1 符号 → 非零
#   A15 失败时旧 dist 不被"假成功"覆盖（逐字节不变）
#   A16 不删除已有 target 内旧产物（旧版末尾 rm -rf musl UI）
#   B1  GNU UI：cargo 失败     → 非零，OUT_DIR 无产物
#   B2  GNU UI：GLIBC 超过基线 → 非零，且**不发布**到 OUT_DIR
#   B3  GNU UI：GLIBCXX 超基线 → 非零
#   B4  GNU UI：interpreter 不是 glibc loader → 非零，且不发布
#   B5  GNU UI：缺 AV1         → 非零，且不发布
#   B6  GNU UI：全部合规       → 三件发布 + RUST-TOOLCHAIN.txt（记录固定工具链）
#   B7  libaom：源码解压到一半（结构不完整）→ 明确报错，不留可误用的缓存
#   B8  libaom：旧缓存结构完整但无标记     → 照常复用，不重新下载
#
# 用法: tools/build_contract_test.sh
# 退出码：全部通过 0，任一失败 1。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIST="${BUILD_DIST_UNDER_TEST:-$SCRIPT_DIR/build-dist.sh}"
BUILD_GNU="${BUILD_GNU_UNDER_TEST:-$SCRIPT_DIR/build-gnu-ui.sh}"
for f in "$BUILD_DIST" "$BUILD_GNU"; do
  [ -f "$f" ] || { echo "FAIL: 找不到 $f"; exit 1; }
done

WORKROOT="$(mktemp -d "${TMPDIR:-/tmp}/sr-buildtest.XXXXXX")"
cleanup() { [ -n "${KEEP_WORKROOT:-}" ] || rm -rf "$WORKROOT"; }
trap cleanup EXIT

VER="0.59.17"
fail=0
total=0
ok()  { total=$((total+1)); echo "  ✓ $*"; }
bad() { total=$((total+1)); fail=1; echo "  ✗ $*"; }

# ── 合成制品生成器（真实 ELF / PE 头，真实 readelf 能读）─────────────
GEN="$WORKROOT/genbin.py"
cat > "$GEN" <<'PY'
import struct, sys

PT_LOAD, PT_DYNAMIC, PT_INTERP = 1, 2, 3
EM = {"x86_64": 62, "aarch64": 183, "armv7": 40}
INTERP = {
    "x86_64": b"/lib64/ld-linux-x86-64.so.2\x00",
    "aarch64": b"/lib/ld-linux-aarch64.so.1\x00",
    "armv7": b"/lib/ld-linux-armhf.so.3\x00",
}
MUSL_INTERP = {
    "x86_64": b"/lib/ld-musl-x86_64.so.1\x00",
    "aarch64": b"/lib/ld-musl-aarch64.so.1\x00",
    "armv7": b"/lib/ld-musl-armhf.so.1\x00",
}

def elf(path, arch, dynamic=True, payload=b"", interp_override=False):
    is64 = arch != "armv7"
    cls = 2 if is64 else 1
    ehsize, phentsize, phoff = (64, 56, 64) if is64 else (52, 32, 52)
    if is64:
        fmt_h = "<HHIQQQIHHHHHH"
        fmt_p = "<IIQQQQQQ"
    else:
        fmt_h = "<HHIIIIIHHHHHH"
        fmt_p = "<IIIIIIII"
    segs = []
    if dynamic:
        segs.append((PT_INTERP, 4,
                     MUSL_INTERP[arch] if interp_override else INTERP[arch]))
        segs.append((PT_DYNAMIC, 6, b"\x00" * (112 if is64 else 88)))
    segs.append((PT_LOAD, 5, b"\x00" * 64))
    n = len(segs)
    e_type = 3 if dynamic else 2
    ident = b"\x7fELF" + bytes([cls, 1, 1, 0]) + b"\x00" * 8
    if is64:
        hdr = ident + struct.pack(fmt_h, e_type, EM[arch], 1, 0, phoff, 0, 0,
                                  ehsize, phentsize, n, 64, 0, 0)
    else:
        hdr = ident + struct.pack(fmt_h, e_type, EM[arch], 1, 0, phoff, 0, 0,
                                  ehsize, phentsize, n, 40, 0, 0)
    base = phoff + phentsize * n
    body, ph = b"", b""
    for t, fl, seg in segs:
        off = base + len(body)
        if is64:
            ph += struct.pack(fmt_p, t, fl, off, 0, 0, len(seg), len(seg), 1)
        else:
            ph += struct.pack(fmt_p, t, off, 0, 0, len(seg), len(seg), fl, 1)
        body += seg
    with open(path, "wb") as f:
        f.write(hdr + ph + body + payload)

def pe(path, subsystem, payload=b""):
    dos = b"MZ" + b"\x00" * 58 + struct.pack("<I", 0x80)
    dos += b"\x00" * (0x80 - len(dos))
    coff = b"PE\x00\x00" + struct.pack("<HHIIIHH", 0x8664, 0, 0, 0, 0, 240, 0x22)
    opt = struct.pack("<H", 0x20B) + b"\x00" * 238
    opt = opt[:68] + struct.pack("<H", subsystem) + opt[70:]
    with open(path, "wb") as f:
        f.write(dos + coff + opt + payload)

if __name__ == "__main__":
    kind, out, ver, av1, arch, mark = sys.argv[1:7]
    payload = ("VER=%s MARK=%s" % (ver, mark)).encode()
    if av1 == "1":
        payload += b" aom_codec_av1_cx"
    if kind == "elf":
        elf(out, arch, dynamic=True, payload=payload)
    elif kind == "elf-musl-interp":
        elf(out, arch, dynamic=True, payload=payload, interp_override=True)
    elif kind == "static":
        elf(out, arch, dynamic=False, payload=payload)
    elif kind == "pe-gui":
        pe(out, 2, payload + (b" requireAdministrator" if av1 == "1" else b""))
    elif kind == "pe-gui-nomanifest":
        pe(out, 2, payload)
    elif kind == "pe-console":
        pe(out, 3, payload)
PY
gen() { python3 "$GEN" "$@"; }

AOM='aom_codec_av1_cx'

# ── 沙箱骨架 ────────────────────────────────────────────────────────
#
# box/
#   repo/      build-dist.sh / build-gnu-ui.sh / Cargo.toml / build.rs / stubs / windows
#   bin/       假 cargo / 假交叉工具链 / 假 docker
#   cache/     libaom 缓存（Windows 静态库 + 各 target）
#   gnu-out/   Linux GNU UI 中间产物
#   dist/      输出目录
new_box() {
  local box="$WORKROOT/$1"
  mkdir -p "$box/repo/tools" "$box/repo/windows" "$box/bin" \
           "$box/cache" "$box/gnu-out" "$box/dist"
  cp "$BUILD_DIST" "$box/repo/tools/build-dist.sh"
  cp "$BUILD_GNU"  "$box/repo/tools/build-gnu-ui.sh"
  chmod +x "$box/repo/tools/"*.sh
  printf '[package]\nname = "shell-remote"\nversion = "%s"\nedition = "2021"\n' "$VER" \
    > "$box/repo/Cargo.toml"
  : > "$box/repo/build.rs"
  echo "int x;" > "$box/repo/tools/glibc-cxx-stubs.c"
  echo "1 ICON MANIFEST" > "$box/repo/windows/embed-manifest.rc"

  # 假 cargo：按 $FAKE_PLAN 描述写出制品（默认全部合规）。
  #   FAKE_PLAN=<file>  每行 "target|binname|kind|arch|ver|av1|mark"
  #   FAKE_WRITE_TO=<dir> 覆盖写入目录（模拟"cargo 写到了别处"）
  #   FAKE_CARGO_FAIL=1   直接失败
  cat > "$box/bin/cargo" <<'CARGO'
#!/bin/bash
set -u
echo "cargo $*" >> "${FAKE_LOG:-/dev/null}"
[ "${FAKE_CARGO_FAIL:-0}" = "1" ] && { echo "error: could not compile" >&2; exit 101; }
target=""; bin=""
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in
    --target) target="${args[i+1]}";;
    --bin)    bin="${args[i+1]}";;
    --version) echo "cargo 9.9.9"; exit 0;;
  esac
done
[ -n "$target" ] || exit 0
out="${FAKE_WRITE_TO:-${CARGO_TARGET_DIR:-$PWD/target}}/$target/release"
mkdir -p "$out"
binname="${bin:-shell-remote}"
[ "$target" = "x86_64-pc-windows-gnu" ] && binname="$binname.exe"
[ "$binname" = "shell-remote-ui.exe" ] || true
[ -f "${FAKE_PLAN:-}" ] || { echo "no plan" >&2; exit 1; }
line="$(grep -F "$target|$binname|" "${FAKE_PLAN}" | tail -1)"
[ -n "$line" ] || { echo "plan missing for $target/$binname" >&2; exit 1; }
IFS='|' read -r _ _ kind arch ver av1 mark <<<"$line"
# kind=absent：本目标"构建成功但没产出文件"，用来验证取件门禁（而不是让
# 假 cargo 直接失败——那只会验证 set -e）。
[ "$kind" = "absent" ] && exit 0
python3 "${FAKE_GEN:?}" "$kind" "$out/$binname" "$ver" "$av1" "$arch" "$mark" || exit 1
CARGO
  chmod +x "$box/bin/cargo"

  # 假只读工具（版本查询用）。
  for t in rustc; do
    printf '#!/bin/bash\necho "%s 9.9.9"\n' "$t" > "$box/bin/$t"
    chmod +x "$box/bin/$t"
  done

  # 假 musl / mingw / GNU 交叉工具链：只要能被 `command -v` 找到且退出 0。
  local p
  for p in musl-gcc musl-g++ x86_64-w64-mingw32-gcc x86_64-w64-mingw32-g++ \
           x86_64-w64-mingw32-ar x86_64-w64-mingw32-windres aarch64-linux-gnu-gcc g++ \
           aarch64-linux-gnu-g++ arm-linux-gnueabihf-gcc arm-linux-gnueabihf-g++ gcc; do
    cat > "$box/bin/$p" <<'TOOL'
#!/bin/bash
case "$1" in
  --version) echo "fakecc 1.0"; exit 0;;
  -print-file-name=*) echo "/fake/libstdc++.a"; exit 0;;
esac
exit 0
TOOL
    chmod +x "$box/bin/$p"
  done
  # mingw ar：生成一个 .o，供 `ar rc` 有输入。
  cat > "$box/bin/x86_64-w64-mingw32-ar" <<'AR'
#!/bin/bash
case "$1" in
  x) mkdir -p x; : > x/fake.o;;
  rc) shift; out="$1"; shift; : > "$out";;
esac
exit 0
AR
  chmod +x "$box/bin/x86_64-w64-mingw32-ar"
  # windres：产出 manifest 目标文件。
  cat > "$box/bin/x86_64-w64-mingw32-windres" <<'WR'
#!/bin/bash
while [ $# -gt 0 ]; do [ "$1" = "-o" ] && out="$2"; shift; done
: > "${out:-manifest.o}"; exit 0
WR
  chmod +x "$box/bin/x86_64-w64-mingw32-windres"
  # 假 docker：本套件走 SKIP_GNU_UI=1，不应被调用。
  cat > "$box/bin/docker" <<'DK'
#!/bin/bash
echo "docker-called" >> "${FAKE_LOG:-/dev/null}"
exit 1
DK
  chmod +x "$box/bin/docker"

  # 假工具链前缀（build-dist 用 <prefix>gcc 等）。
  mkdir -p "$box/muslcc-aarch64/bin" "$box/muslcc-armv7/bin"
  for p in "$box/muslcc-aarch64/bin/aarch64-linux-musl-" "$box/muslcc-armv7/bin/arm-linux-musleabihf-"; do
    for t in gcc g++ ar; do
      printf '#!/bin/bash\nexit 0\n' > "$p$t"; chmod +x "$p$t"
    done
  done

  # Windows 静态 libaom（预置，A10 会删掉它验证前置门禁）。
  mkdir -p "$box/cache/libaom-x86_64-pc-windows-gnu/lib"
  : > "$box/cache/libaom-x86_64-pc-windows-gnu/lib/libaom.a"
  echo "$box"
}

# 默认 FAKE_PLAN：八件全合规。
write_plan() { # $1=box  $2..=override lines "target|bin|kind|arch|ver|av1|mark"
  local box="$1"; shift
  local plan="$box/plan.txt"
  : > "$plan"
  cat >> "$plan" <<EOF
x86_64-unknown-linux-musl|shell-remote|static|x86_64|$VER|0|cli64
aarch64-unknown-linux-musl|shell-remote|static|aarch64|$VER|0|cliA
armv7-unknown-linux-musleabihf|shell-remote|static|armv7|$VER|0|cliV
x86_64-pc-windows-gnu|shell-remote.exe|pe-console|x86_64|$VER|0|cliW
x86_64-pc-windows-gnu|shell-remote-ui.exe|pe-gui|x86_64|$VER|1|uiW
EOF
  # 覆盖行（同 key 后写优先，由 sed 处理顺序：附加在末尾并让后面的覆盖前面）。
  local line
  for line in "$@"; do echo "$line" >> "$plan"; done
  echo "$plan"
}

# 预置 GNU UI 三件（默认合规动态 ELF）与 dist/ 旧内容。
seed_gnu_out() { # $1=box  [$2..=override kind per arch]
  local box="$1"; shift
  gen elf "$box/gnu-out/shell-remote-ui-x86_64" "$VER" 1 x86_64 gnu64
  gen elf "$box/gnu-out/shell-remote-ui-aarch64" "$VER" 1 aarch64 gnuA
  gen elf "$box/gnu-out/shell-remote-ui-armv7" "$VER" 1 armv7 gnuV
  chmod +x "$box/gnu-out"/shell-remote-ui-*
}
seed_old_dist() { # $1=box → 八件旧内容（内容可辨认）
  local box="$1"
  local n
  for n in shell-remote-x86_64 shell-remote-aarch64 shell-remote-armv7 \
           shell-remote-x86_64.exe shell-remote-ui-x86_64 shell-remote-ui-aarch64 \
           shell-remote-ui-armv7 shell-remote-ui-x86_64.exe; do
    printf 'OLD-%s\n' "$n" > "$box/dist/$n"
  done
}
dist_fingerprint() { # $1=box → 排序后的 name:sha256 列表（只算八件）
  local box="$1" n
  for n in shell-remote-x86_64 shell-remote-aarch64 shell-remote-armv7 \
           shell-remote-x86_64.exe shell-remote-ui-x86_64 shell-remote-ui-aarch64 \
           shell-remote-ui-armv7 shell-remote-ui-x86_64.exe; do
    if [ -f "$box/dist/$n" ]; then
      printf '%s:%s\n' "$n" "$(sha256sum "$box/dist/$n" | cut -d' ' -f1)"
    else
      printf '%s:MISSING\n' "$n"
    fi
  done | sort
}

# 在沙箱里跑 build-dist.sh。
#   $1=box  其余=额外 env 赋值（k=v 形式，供 env 使用）
run_dist() {
  local box="$1"; shift
  (
    cd "$box/repo" || exit 1
    PATH="$box/bin:$PATH" \
      FAKE_GEN="$GEN" \
      FAKE_PLAN="${FAKE_PLAN:-$box/plan.txt}" \
      FAKE_LOG="$box/fake.log" \
      CACHE_DIR="$box/cache" \
      DIST_DIR="$box/dist" \
      GNU_OUT_DIR="$box/gnu-out" \
      MUSLCC_AARCH64="$box/muslcc-aarch64/bin/aarch64-linux-musl-" \
      MUSLCC_ARMV7="$box/muslcc-armv7/bin/arm-linux-musleabihf-" \
      SKIP_GNU_UI="${SKIP_GNU_UI:-1}" \
      env "$@" bash tools/build-dist.sh
  ) > "$box/out.log" 2>&1
  echo $?
}

# 标准前置：写好 plan、预置 GNU UI 与旧 dist。
prep_ok_box() { # $1=box [overrides...]
  local box="$1"; shift
  write_plan "$box" "$@" >/dev/null
  seed_gnu_out "$box"
  seed_old_dist "$box"
}

echo "── 构建合同测试（$WORKROOT）──"
echo
echo "A. tools/build-dist.sh"

# ── A1 正常路径 ────────────────────────────────────────────────────
{
  box="$(new_box a1-happy)"
  prep_ok_box "$box"
  rc="$(run_dist "$box")"
  if [ "$rc" -eq 0 ]; then ok "A1 正常路径：退出 0"; else bad "A1 正常路径：退出 $rc"; sed -n '1,25p' "$box/out.log"; fi
  n=0
  for f in "$box/dist"/shell-remote*; do [ -f "$f" ] && n=$((n+1)); done
  [ "$n" -eq 8 ] && ok "A1 dist/ 八件齐全" || bad "A1 dist/ 只有 $n 件"
  # 逐字节等于各自来源
  same=1
  cmp -s "$box/dist/shell-remote-x86_64" "$box/repo/target/x86_64-unknown-linux-musl/release/shell-remote" || same=0
  cmp -s "$box/dist/shell-remote-ui-aarch64" "$box/gnu-out/shell-remote-ui-aarch64" || same=0
  cmp -s "$box/dist/shell-remote-ui-x86_64.exe" "$box/repo/target/x86_64-pc-windows-gnu/release/shell-remote-ui.exe" || same=0
  [ "$same" -eq 1 ] && ok "A1 dist/ 与各自来源逐字节一致" || bad "A1 dist/ 与来源不一致"
  grep -q 'MARK=gnu64' "$box/dist/shell-remote-ui-x86_64" \
    && ok "A1 Linux UI 取的是 GNU 目录产物" || bad "A1 Linux UI 来源不对"
  [ -f "$box/dist/BUILD-INFO.txt" ] && ok "A1 导出 BUILD-INFO.txt" || bad "A1 没有 BUILD-INFO.txt"
  grep -q "source_commit:" "$box/dist/BUILD-INFO.txt" && ok "A1 BUILD-INFO 记录源码" \
    || bad "A1 BUILD-INFO 缺源码记录"
  # 无 staging 残留
  if ls -a "$box/dist" | grep -q '\.staging'; then bad "A1 残留 staging 目录"; else ok "A1 无 staging 残留"; fi
  grep -q docker-called "$box/fake.log" 2>/dev/null && bad "A1 SKIP_GNU_UI 仍调了 docker" \
    || ok "A1 SKIP_GNU_UI 未启动容器"
}

# ── A2 缺目标文件 ──────────────────────────────────────────────────
{
  box="$(new_box a2-missing)"
  prep_ok_box "$box"
  before="$(dist_fingerprint "$box")"
  # 让 cargo 对 armv7 CLI "成功但没产出文件"：把该行改写为 kind=absent。
  sed -E 's#^(armv7-unknown-linux-musleabihf\|shell-remote\|)static#\1absent#' \
      "$box/plan.txt" > "$box/plan2.txt"
  FAKE_PLAN="$box/plan2.txt"; export FAKE_PLAN
  rc="$(run_dist "$box")"
  unset FAKE_PLAN
  [ "$rc" -ne 0 ] && ok "A2 缺目标文件：非零退出（$rc）" || bad "A2 缺目标文件却退出 0"
  grep -q '缺目标文件' "$box/out.log" && ok "A2 指出缺哪个目标文件" || bad "A2 未指出缺失文件"
  [ "$before" = "$(dist_fingerprint "$box")" ] && ok "A2 dist/ 完全未改动" \
    || bad "A2 dist/ 被改动了"
}

# ── A3 版本不符 ────────────────────────────────────────────────────
{
  box="$(new_box a3-version)"
  prep_ok_box "$box" "x86_64-pc-windows-gnu|shell-remote-ui.exe|pe-gui|x86_64|0.59.18|1|uiW"
  before="$(dist_fingerprint "$box")"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A3 版本不符：非零退出（$rc）" || bad "A3 版本不符却退出 0"
  grep -q '版本串不含' "$box/out.log" && ok "A3 报出版本不符" || bad "A3 未报版本不符"
  [ "$before" = "$(dist_fingerprint "$box")" ] && ok "A3 dist/ 未改动" || bad "A3 dist/ 被改动"
}

# ── A4 架构不符 ────────────────────────────────────────────────────
{
  box="$(new_box a4-arch)"
  prep_ok_box "$box" "aarch64-unknown-linux-musl|shell-remote|static|x86_64|$VER|0|cliA"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A4 架构不符：非零退出（$rc）" || bad "A4 架构不符却退出 0"
  grep -q '架构错' "$box/out.log" && ok "A4 报出架构错" || bad "A4 未报架构错"
}

# ── A5 Linux UI 是 musl 静态 ───────────────────────────────────────
{
  box="$(new_box a5-static-ui)"
  prep_ok_box "$box"
  gen static "$box/gnu-out/shell-remote-ui-armv7" "$VER" 1 armv7 gnuV
  chmod +x "$box/gnu-out/shell-remote-ui-armv7"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A5 静态 Linux UI：非零退出（$rc）" || bad "A5 静态 Linux UI 被接受"
  grep -q '不是动态链接' "$box/out.log" && ok "A5 明确指出不是动态链接" || bad "A5 没有说明原因"
}

# ── A6 Linux UI 缺 AV1 ─────────────────────────────────────────────
{
  box="$(new_box a6-gnu-noav1)"
  prep_ok_box "$box"
  gen elf "$box/gnu-out/shell-remote-ui-x86_64" "$VER" 0 x86_64 gnu64
  chmod +x "$box/gnu-out/shell-remote-ui-x86_64"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A6 Linux UI 缺 AV1：非零退出（$rc）" || bad "A6 缺 AV1 仍成功"
  grep -q 'AV1 编码器符号' "$box/out.log" && ok "A6 报出缺 AV1" || bad "A6 未报缺 AV1"
}

# ── A7 Windows UI 缺 AV1 ───────────────────────────────────────────
{
  box="$(new_box a7-win-noav1)"
  prep_ok_box "$box" "x86_64-pc-windows-gnu|shell-remote-ui.exe|pe-gui|x86_64|$VER|0|uiW"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A7 Windows UI 缺 AV1：非零退出（$rc）" || bad "A7 Windows UI 缺 AV1 仍成功"
  grep -q 'AV1 编码器符号' "$box/out.log" && ok "A7 报出 Windows UI 缺 AV1" \
    || bad "A7 未报 Windows UI 缺 AV1"
}

# ── A8 Windows UI subsystem 错 ─────────────────────────────────────
{
  box="$(new_box a8-win-sub)"
  prep_ok_box "$box" "x86_64-pc-windows-gnu|shell-remote-ui.exe|pe-console|x86_64|$VER|1|uiW"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A8 Windows UI 是 console：非零退出（$rc）" || bad "A8 subsystem 错却成功"
  grep -q 'subsystem 错' "$box/out.log" && ok "A8 报出 subsystem 错" || bad "A8 未报 subsystem 错"
}

# ── A9 Windows UI 缺 admin manifest ────────────────────────────────
{
  box="$(new_box a9-win-manifest)"
  # GUI subsystem、带 AV1，但生成器变体不写 requireAdministrator。
  prep_ok_box "$box" "x86_64-pc-windows-gnu|shell-remote-ui.exe|pe-gui-nomanifest|x86_64|$VER|1|uiW"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A9 缺 admin manifest：非零退出（$rc）" || bad "A9 缺 manifest 仍成功"
  grep -q 'requireAdministrator' "$box/out.log" && ok "A9 报出缺 manifest" || bad "A9 未报缺 manifest"
}

# ── A10 Windows libaom 缺失 → 开工前失败 ───────────────────────────
{
  box="$(new_box a10-win-aom)"
  prep_ok_box "$box"
  rm -rf "$box/cache/libaom-x86_64-pc-windows-gnu"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A10 Windows 缺 libaom：非零退出（$rc）" || bad "A10 缺 libaom 仍成功"
  grep -q 'Windows UI 缺少静态 libaom' "$box/out.log" && ok "A10 明确报出前置缺失" \
    || bad "A10 没有明确报出前置缺失"
  if grep -q '^cargo build' "$box/fake.log" 2>/dev/null; then
    bad "A10 前置已失败却仍进入编译"
  else
    ok "A10 开工前失败（未进入任何编译）"
  fi
  # 且不允许 WIN_LIBXAOM_DIR 留空绕过
  rm -rf "$box/cache/libaom-x86_64-pc-windows-gnu"
  rc="$(run_dist "$box" WIN_LIBXAOM_DIR="$box/nowhere")"
  [ "$rc" -ne 0 ] && ok "A10 显式指向不存在的 libaom 目录仍失败" \
    || bad "A10 指向不存在目录却成功"
}

# ── A11 CARGO_TARGET_DIR 覆盖 → 取新目录的新文件 ──────────────────
{
  box="$(new_box a11-target-dir)"
  prep_ok_box "$box"
  # 先在默认 target 里放一批"旧"制品（正确形态、旧 marker），再把新构建写到别的目录。
  mkdir -p "$box/repo/target/x86_64-unknown-linux-musl/release"
  gen static "$box/repo/target/x86_64-unknown-linux-musl/release/shell-remote" "$VER" 0 x86_64 STALE64
  alt="$box/alt-target"
  mkdir -p "$alt"
  # plan 里的 marker 改成 NEW，用于区分。
  sed -E 's/\|cli64$/|NEW64/' "$box/plan.txt" > "$box/plan-alt.txt"
  FAKE_PLAN="$box/plan-alt.txt"; export FAKE_PLAN
  rc="$(run_dist "$box" CARGO_TARGET_DIR="$alt")"
  unset FAKE_PLAN
  [ "$rc" -eq 0 ] && ok "A11 CARGO_TARGET_DIR 覆盖：退出 0" \
    || { bad "A11 CARGO_TARGET_DIR 覆盖：退出 $rc"; sed -n '1,25p' "$box/out.log"; }
  if grep -qa 'MARK=NEW64' "$box/dist/shell-remote-x86_64" 2>/dev/null; then
    ok "A11 取的是覆盖目录里的新文件"
  else
    bad "A11 取的不是新目录文件（可能又从 \$ROOT/target 拿了旧文件）"
  fi
  [ -f "$alt/x86_64-unknown-linux-musl/release/shell-remote" ] \
    && ok "A11 新文件确实落在覆盖目录" || bad "A11 覆盖目录没有产物"
}

# ── A12 cargo 写到别处（旧 bug 复现）→ 必须失败而不是拿旧文件 ────
{
  box="$(new_box a12-wrong-dir)"
  prep_ok_box "$box"
  # 默认 target 里预置"看起来正确"的旧制品；cargo 却写到 $box/elsewhere。
  mkdir -p "$box/repo/target/x86_64-unknown-linux-musl/release"
  gen static "$box/repo/target/x86_64-unknown-linux-musl/release/shell-remote" "$VER" 0 x86_64 STALE64
  before="$(dist_fingerprint "$box")"
  rc="$(run_dist "$box" FAKE_WRITE_TO="$box/elsewhere" CARGO_TARGET_DIR="$box/empty-target")"
  [ "$rc" -ne 0 ] && ok "A12 cargo 写到别处：非零退出（$rc）" || bad "A12 仍报成功"
  grep -q '缺目标文件' "$box/out.log" && ok "A12 报出取件路径无文件" \
    || bad "A12 没有把"取件路径无文件"说清楚"
  [ "$before" = "$(dist_fingerprint "$box")" ] && ok "A12 dist/ 未被静默替换成旧文件" \
    || bad "A12 dist/ 被旧文件顶上了"
}

# ── A13 staging 中途复制失败 ───────────────────────────────────────
{
  box="$(new_box a13-stage-fail)"
  prep_ok_box "$box"
  before="$(dist_fingerprint "$box")"
  # 假 cp：复制 shell-remote-ui-armv7 时失败（模拟磁盘满/权限/ETXTBSY）。
  mkdir -p "$box/failbin"
  cat > "$box/failbin/cp" <<'FAKECP'
#!/bin/bash
for a in "$@"; do
  case "$a" in
    */shell-remote-ui-armv7) echo "cp: cannot create: No space left on device" >&2; exit 1;;
  esac
done
exec /bin/cp "$@"
FAKECP
  chmod +x "$box/failbin/cp"
  rc="$(run_dist "$box" PATH="$box/failbin:$box/bin:$PATH")"
  [ "$rc" -ne 0 ] && ok "A13 staging 复制失败：非零退出（$rc）" || bad "A13 复制失败仍退出 0"
  grep -q '复制失败' "$box/out.log" && ok "A13 报出复制失败" || bad "A13 未报复制失败"
  [ "$before" = "$(dist_fingerprint "$box")" ] && ok "A13 dist/ 保持上一版（无新旧混合）" \
    || bad "A13 dist/ 出现新旧混合"
  if ls -a "$box/dist" | grep -q '\.staging'; then bad "A13 残留 staging 目录"; else ok "A13 staging 已清理"; fi
}

# ── A14 lean CLI 里出现 AV1 符号 ───────────────────────────────────
{
  box="$(new_box a14-cli-av1)"
  prep_ok_box "$box" "x86_64-unknown-linux-musl|shell-remote|static|x86_64|$VER|1|cli64"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A14 lean CLI 含 AV1：非零退出（$rc）" || bad "A14 lean CLI 含 AV1 仍成功"
  grep -q '不应包含 AV1' "$box/out.log" && ok "A14 报出 lean CLI 不该有 AV1" \
    || bad "A14 未报出 lean CLI 含 AV1"
}

# ── A15 失败不得"假成功"覆盖旧 dist（逐字节）──────────────────────
{
  box="$(new_box a15-no-fake-success)"
  prep_ok_box "$box"
  before="$(dist_fingerprint "$box")"
  # 破坏 Windows UI 的 AV1，其余都正常 → 一个失败就整体失败。
  gen pe-gui "$box/repo/target-x.exe" "$VER" 1 x86_64 tmp   # 无关，只为确认生成器可用
  sed -E 's/\|uiW$/|uiW/' "$box/plan.txt" > /dev/null
  sed -i -E 's#(x86_64-pc-windows-gnu\|shell-remote-ui.exe\|pe-gui\|x86_64\|[^|]+\|)1\|uiW#\10|uiW#' "$box/plan.txt"
  rc="$(run_dist "$box")"
  [ "$rc" -ne 0 ] && ok "A15 任一件失败：整体非零退出（$rc）" || bad "A15 部分失败仍报成功"
  after="$(dist_fingerprint "$box")"
  [ "$before" = "$after" ] && ok "A15 旧 dist 逐字节未变（没有假成功覆盖）" \
    || bad "A15 旧 dist 被部分覆盖"
}

# ── A16 不删除已有 target 里的旧产物 ───────────────────────────────
{
  box="$(new_box a16-keep-target)"
  prep_ok_box "$box"
  # 旧版会在末尾 rm -rf 这些 musl UI 产物；现在必须原样保留。
  for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf; do
    mkdir -p "$box/repo/target/$t/release"
    printf 'OLD-MUSL-UI\n' > "$box/repo/target/$t/release/shell-remote-ui"
  done
  rc="$(run_dist "$box")"
  [ "$rc" -eq 0 ] && ok "A16 正常构建退出 0" || bad "A16 退出 $rc"
  kept=1
  for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf; do
    grep -q 'OLD-MUSL-UI' "$box/repo/target/$t/release/shell-remote-ui" 2>/dev/null || kept=0
  done
  [ "$kept" -eq 1 ] && ok "A16 已有 target 旧产物未被删除" || bad "A16 旧产物被删掉了"
}

echo
echo "B. tools/build-gnu-ui.sh --inside"

# ── B 套件：--inside 用假 readelf 控制 ABI 符号 ────────────────────
#
# build-gnu-ui.sh --inside 只做三件事：构建（cargo）→ 门禁（readelf/strings）
# → 发布到 OUT_DIR。这里用假 cargo 产出真实 ELF（架构/interp 真），用假
# readelf 控制 GLIBC/GLIBCXX 版本符号（宿主 glibc 是 2.4x，造不出 2.35 的
# 真实符号，只能控制这一层）；架构与 AV1 仍然由真实 ELF 内容决定。
new_gnu_box() {
  local box="$WORKROOT/$1"
  mkdir -p "$box/repo/tools" "$box/bin" "$box/cache" "$box/out" "$box/target"
  cp "$BUILD_GNU" "$box/repo/tools/build-gnu-ui.sh"
  printf '[package]\nname = "shell-remote"\nversion = "%s"\nedition = "2021"\n' "$VER" \
    > "$box/repo/Cargo.toml"
  cat > "$box/bin/cargo" <<'CARGO'
#!/bin/bash
set -u
[ "${FAKE_CARGO_FAIL:-0}" = "1" ] && { echo "error[E0308]: fake build error" >&2; exit 101; }
target=""
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  [ "${args[i]}" = "--target" ] && target="${args[i+1]}"
done
[ -n "$target" ] || exit 0
out="${CARGO_TARGET_DIR:?}/$target/release"
mkdir -p "$out"
arch="x86_64"
case "$target" in aarch64*) arch=aarch64;; armv7*) arch=armv7;; esac
python3 "$FAKE_GEN" "${FAKE_KIND:-elf}" "$out/shell-remote-ui" "${FAKE_VER:-0.59.17}" \
  "${FAKE_AV1:-1}" "$arch" gnu || exit 1
CARGO
  chmod +x "$box/bin/cargo"
  # 假 readelf：只用于 --version-info / -l / -d 三个查询。
  cat > "$box/bin/readelf" <<'RE'
#!/bin/bash
case "$1" in
  --version-info)
    printf 'Version needs section %s:\n' ".gnu.version_r"
    [ -n "${FAKE_GLIBC:-}" ]  && printf '  0x0001: Name: GLIBC_%s  Flags: none  Version: 4\n' "$FAKE_GLIBC"
    [ -n "${FAKE_GLIBC2:-}" ] && printf '  0x0002: Name: GLIBC_%s  Flags: none  Version: 3\n' "$FAKE_GLIBC2"
    [ -n "${FAKE_GLIBCXX:-}" ] && printf '  0x0003: Name: GLIBCXX_%s  Flags: none  Version: 2\n' "$FAKE_GLIBCXX"
    exit 0;;
  -d)
    printf ' 0x0000000000000001 (NEEDED)  Shared library: [%s]\n' "${FAKE_NEEDED:-libc.so.6}"
    exit 0;;
esac
exec /usr/bin/readelf "$@"
RE
  chmod +x "$box/bin/readelf"
  for p in gcc g++ ar aarch64-linux-gnu-gcc aarch64-linux-gnu-g++ aarch64-linux-gnu-ar \
           arm-linux-gnueabihf-gcc arm-linux-gnueabihf-g++ arm-linux-gnueabihf-ar; do
    printf '#!/bin/bash\nexit 0\n' > "$box/bin/$p"; chmod +x "$box/bin/$p"
  done
  mkdir -p "$box/cache/libaom-x86_64-unknown-linux-gnu/lib" \
           "$box/cache/libaom-aarch64-unknown-linux-gnu/lib" \
           "$box/cache/libaom-armv7-unknown-linux-gnueabihf/lib"
  for d in "$box/cache"/libaom-*/lib; do : > "$d/libaom.a"; done
  # 半成品缓存探测需要"已解压完成"标记；默认给一份有效的。
  mkdir -p "$box/cache/libaom-src/aom"
  : > "$box/cache/libaom-src/CMakeLists.txt"
  : > "$box/cache/libaom-src/aom/aom_codec.h"
  : > "$box/cache/libaom-src/aom/aomcx.h"
  : > "$box/cache/libaom-src/.extracted-ok"
  echo "$box"
}

run_gnu_inside() { # $1=box  rest=env k=v
  local box="$1"; shift
  (
    cd "$box/repo" || exit 1
    PATH="$box/bin:$PATH" \
      FAKE_GEN="$GEN" \
      FAKE_LOG="$box/fake.log" \
      CARGO_TARGET_DIR="$box/target" \
      OUT_DIR="$box/out" \
      CACHE_DIR="$box/cache" \
      GNU_RUST_TOOLCHAIN=9.9.9 \
      env "$@" bash tools/build-gnu-ui.sh --inside
  ) > "$box/out.log" 2>&1
  echo $?
}
gnu_artifact_count() { local n=0 f; for f in "$1/out"/shell-remote-ui-*; do [ -f "$f" ] && n=$((n+1)); done; echo "$n"; }

# ── B1 cargo 失败 ──────────────────────────────────────────────────
{
  box="$(new_gnu_box b1-cargo-fail)"
  rc="$(run_gnu_inside "$box" FAKE_CARGO_FAIL=1 FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4)"
  [ "$rc" -ne 0 ] && ok "B1 cargo 失败：非零退出（$rc）" || bad "B1 cargo 失败却退出 0"
  [ "$(gnu_artifact_count "$box")" -eq 0 ] && ok "B1 失败时不发布任何产物" \
    || bad "B1 失败却往 OUT_DIR 发布了产物"
  grep -q 'cargo build 失败' "$box/out.log" && ok "B1 报出构建失败" || bad "B1 未报构建失败"
}

# ── B2 GLIBC 超过基线 ──────────────────────────────────────────────
{
  box="$(new_gnu_box b2-glibc-high)"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.36 FAKE_GLIBCXX=3.4)"
  [ "$rc" -ne 0 ] && ok "B2 GLIBC 2.36：非零退出（$rc）" || bad "B2 GLIBC 2.36 仍成功"
  grep -q '高于基线' "$box/out.log" && ok "B2 报出超过基线" || bad "B2 未报超基线"
  [ "$(gnu_artifact_count "$box")" -eq 0 ] && ok "B2 超基线时不发布产物" \
    || bad "B2 超基线仍发布了产物（旧版先 mv 再检查）"
}

# ── B3 GLIBCXX 超过基线 ────────────────────────────────────────────
{
  box="$(new_gnu_box b3-glibcxx-high)"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4.31)"
  [ "$rc" -ne 0 ] && ok "B3 GLIBCXX 3.4.31：非零退出（$rc）" || bad "B3 GLIBCXX 超基线仍成功"
}

# ── B4 interpreter 不符 ────────────────────────────────────────────
{
  box="$(new_gnu_box b4-interp)"
  # 产物是真 ELF，interpreter 由生成器写死成 musl 的 loader；
  # GNU UI 门禁必须认出"这不是 glibc 动态产物"。
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4 FAKE_KIND=elf-musl-interp)"
  [ "$rc" -ne 0 ] && ok "B4 interpreter 是 musl：非零退出（$rc）" || bad "B4 musl interpreter 仍成功"
  grep -q 'interpreter=' "$box/out.log" && ok "B4 报出 interpreter 不符" || bad "B4 未报 interpreter 不符"
  [ "$(gnu_artifact_count "$box")" -eq 0 ] && ok "B4 不发布不符产物" || bad "B4 发布了不符产物"
}

# ── B5 缺 AV1（真实 strings 判定）───────────────────────────────────
{
  box="$(new_gnu_box b5-noav1)"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4 FAKE_AV1=0)"
  [ "$rc" -ne 0 ] && ok "B5 GNU UI 缺 AV1：非零退出（$rc）" || bad "B5 缺 AV1 仍成功"
  grep -q 'AV1 编码器符号' "$box/out.log" && ok "B5 报出缺 AV1" || bad "B5 未报缺 AV1"
}

# ── B6 全部合规 → 三件发布 + 工具链记录 ────────────────────────────
{
  box="$(new_gnu_box b6-happy)"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4)"
  [ "$rc" -eq 0 ] && ok "B6 全部合规：退出 0" \
    || { bad "B6 全部合规：退出 $rc"; tail -20 "$box/out.log"; }
  [ "$(gnu_artifact_count "$box")" -eq 3 ] && ok "B6 三件 GNU UI 全部发布" \
    || bad "B6 只发布了 $(gnu_artifact_count "$box") 件"
  [ -f "$box/out/RUST-TOOLCHAIN.txt" ] && ok "B6 记录工具链" || bad "B6 缺工具链记录"
  grep -q 'rust_toolchain:  9.9.9' "$box/out/RUST-TOOLCHAIN.txt" \
    && ok "B6 记录的是固定的 Rust 版本" || bad "B6 工具链记录不含固定版本"
}

# ── B7 半成品 libaom 缓存不被复用 ──────────────────────────────────
{
  box="$(new_gnu_box b7-half-aom)"
  # 让 libaom 未构建（删掉库目录）→ 触发走"构建 libaom"分支；
  # 同时把已解压源码弄成"解压到一半"（缺 CMakeLists.txt / 关键头文件），
  # tarball 也弄成截断的。
  for d in "$box/cache"/libaom-x86_64-unknown-linux-gnu "$box/cache"/libaom-aarch64-unknown-linux-gnu \
           "$box/cache"/libaom-armv7-unknown-linux-gnueabihf; do
    rm -rf "$d"
  done
  rm -f "$box/cache/libaom-src/.extracted-ok" "$box/cache/libaom-src/CMakeLists.txt"
  printf 'not-a-gzip' > "$box/cache/libaom.tar.gz"
  # 阻断再下载：把 curl 换成必定失败（避免测试联网）。
  cat > "$box/bin/curl" <<'CURL'
#!/bin/bash
echo "curl: (6) Could not resolve host" >&2
exit 6
CURL
  chmod +x "$box/bin/curl"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4)"
  [ "$rc" -ne 0 ] && ok "B7 libaom 缓存损坏：非零退出（$rc）" || bad "B7 缓存损坏仍成功"
  # 关键：不得留下"看似可用"的源码目录 / 库目录
  if [ -f "$box/cache/libaom-src/.extracted-ok" ]; then
    bad "B7 留下了"已解压"标记（下次会误用半成品）"
  else
    ok "B7 未留下可被误用的解压标记"
  fi
  for d in "$box/cache"/libaom-x86_64-unknown-linux-gnu "$box/cache"/libaom-aarch64-unknown-linux-gnu; do
    [ -f "$d/lib/libaom.a" ] && bad "B7 留下了半成品库 $d" && break
  done
  ok "B7 未留下半成品 libaom 库"
  grep -q '结构不完整' "$box/out.log" && ok "B7 明确指出已有源码结构不完整" \
    || bad "B7 没有把"源码结构不完整"说清楚"
}

# ── B8 旧缓存（无标记但结构完整）照常复用，不误判为半成品 ──────────
{
  box="$(new_gnu_box b8-legacy-cache)"
  # 上一轮次留下的 libaom-src 没有 .extracted-ok 标记，但结构完整：
  # 必须复用（"旧缓存可读复制"），不得重新下载/重新解压。
  rm -f "$box/cache/libaom-src/.extracted-ok"
  cat > "$box/bin/curl" <<'CURL'
#!/bin/bash
echo "curl-called" >> "${FAKE_LOG:-/dev/null}"
echo "curl: (6) Could not resolve host" >&2
exit 6
CURL
  chmod +x "$box/bin/curl"
  # 让其中一个目标缺 libaom.a → 会走"构建 libaom"分支，从而真的检查源码复用。
  rm -rf "$box/cache/libaom-x86_64-unknown-linux-gnu"
  # cmake 用假实现：直接产出 libaom.a，验证源码目录被用上了。
  cat > "$box/bin/cmake" <<'CMAKE'
#!/bin/bash
src="$1"; shift
while [ $# -gt 0 ]; do
  case "$1" in -DCMAKE_INSTALL_PREFIX=*) pre="${1#*=}";; esac
  shift
done
echo "CMAKE_SRC=$src" >> "${FAKE_LOG:-/dev/null}"
mkdir -p "$pre/lib" "$pre/include"
: > "$pre/lib/libaom.a"
exit 0
CMAKE
  chmod +x "$box/bin/cmake"
  cat > "$box/bin/make" <<'MAKE'
#!/bin/bash
exit 0
MAKE
  chmod +x "$box/bin/make"
  rc="$(run_gnu_inside "$box" FAKE_GLIBC=2.35 FAKE_GLIBCXX=3.4)"
  [ "$rc" -eq 0 ] && ok "B8 旧缓存（结构完整、无标记）可直接复用" \
    || { bad "B8 旧缓存被误判为半成品（退出 $rc）"; tail -15 "$box/out.log"; }
  grep -q 'curl-called' "$box/fake.log" 2>/dev/null \
    && bad "B8 结构完整却仍重新下载" || ok "B8 未触发重新下载"
  grep -q "CMAKE_SRC=$box/cache/libaom-src" "$box/fake.log" 2>/dev/null \
    && ok "B8 用的是既有源码目录" || bad "B8 没有用既有源码目录"
}

echo
if [ "$fail" -eq 0 ]; then
  echo "PASS（$total 项断言）"
else
  echo "FAIL（$total 项断言中有失败）"
fi
exit "$fail"
