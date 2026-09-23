#!/bin/sh
# shell-remote agent 安装器
#
#   固定运行账户 + systemd 托管 + 开机自启 + 固定 session-id/key + 预置环境变量
#
# 用法（root）：
#   ./install.sh
#   SR_SESSION_ID=node01 SR_KEY=$(openssl rand -hex 24) ./install.sh
#
# 可覆盖的变量（默认值见下）：
#   SR_USER          运行账户名            sr-agent
#   SR_SERVICE_NAME  systemd 服务名        shell-remote-agent
#   SR_BIN           二进制安装路径        /usr/local/bin/shell-remote
#   SR_CONF_DIR      配置目录              /etc/shell-remote
#   SR_RELAY_URL     relay 地址            https://sshx.zztweb.top
#   SR_SESSION_ID    固定会话 ID           由主机名推导（5-20 位字母数字）
#   SR_KEY           固定鉴权 key          随机生成并打印
#   SR_SHELL         远端终端 shell        /bin/bash
#   SR_ROOT          文件管理器根目录      运行账户的 home
#
# 幂等：已存在的 agent.env 不会被覆盖（要重写加 --force）。
set -eu

SR_USER="${SR_USER:-sr-agent}"
SR_SERVICE_NAME="${SR_SERVICE_NAME:-shell-remote-agent}"
SR_BIN="${SR_BIN:-/usr/local/bin/shell-remote}"
SR_CONF_DIR="${SR_CONF_DIR:-/etc/shell-remote}"
SR_RELAY_URL="${SR_RELAY_URL:-https://sshx.zztweb.top}"
SR_SHELL="${SR_SHELL:-/bin/bash}"
SR_KEY="${SR_KEY:-}"
SR_SESSION_ID="${SR_SESSION_ID:-}"

FORCE=0
for a in "$@"; do
    [ "$a" = "--force" ] && FORCE=1
done

HERE=$(cd "$(dirname "$0")" && pwd)
TPL="$HERE/templates"
SR_CONF="$SR_CONF_DIR/agent.env"
SR_UNIT="/etc/systemd/system/${SR_SERVICE_NAME}.service"

die() { echo "[install] 错误: $*" >&2; exit 1; }
log() { echo "[install] $*"; }

[ "$(id -u)" = 0 ] || die "请以 root 运行（需要创建账户、写 /etc/systemd/system）"
[ -f "$TPL/shell-remote-agent.service" ] || die "缺少模板 $TPL/shell-remote-agent.service"

# ── 1. 运行账户 ────────────────────────────────────────────────────────
if id "$SR_USER" >/dev/null 2>&1; then
    log "账户 $SR_USER 已存在，复用"
else
    log "创建系统账户 $SR_USER"
    useradd --system --create-home --shell "$SR_SHELL" "$SR_USER"
fi
SR_HOME=$(getent passwd "$SR_USER" | cut -d: -f6)
[ -n "$SR_HOME" ] || SR_HOME="/home/$SR_USER"
SR_ROOT="${SR_ROOT:-$SR_HOME}"

# ── 2. 二进制 ──────────────────────────────────────────────────────────
if [ -x "$SR_BIN" ]; then
    SR_VER=$("$SR_BIN" --version 2>/dev/null || echo "版本未知")
    log "二进制已存在：$SR_BIN（$SR_VER）"
else
    log "下载 shell-remote 到 $SR_BIN"
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT INT TERM
    # 官方一键脚本的 --download-only：只落盘到当前目录的 ./shell-remote，不执行
    ( cd "$TMP" && curl -fsSL "$SR_RELAY_URL/agent/install" | sh -s -- --download-only ) \
        || die "下载失败（relay $SR_RELAY_URL 不可达？）"
    [ -s "$TMP/shell-remote" ] || die "下载产物为空"
    install -d -m 0755 "$(dirname "$SR_BIN")"
    install -m 0755 "$TMP/shell-remote" "$SR_BIN"
    rm -rf "$TMP"; trap - EXIT INT TERM
fi

# ── 3. 会话 ID 与 key ──────────────────────────────────────────────────
if [ -z "$SR_SESSION_ID" ]; then
    # 主机名 → 只保留字母数字，截断到 20 位；不足 5 位补 0
    SR_SESSION_ID=$(hostname | tr -cd 'A-Za-z0-9' | cut -c1-20)
    while [ ${#SR_SESSION_ID} -lt 5 ]; do SR_SESSION_ID="${SR_SESSION_ID}0"; done
fi
# 与 proto::is_valid_custom_session_id 同一规则
case "$SR_SESSION_ID" in
    *[!A-Za-z0-9]*) die "SR_SESSION_ID 只能含 ASCII 字母数字：$SR_SESSION_ID" ;;
esac
[ ${#SR_SESSION_ID} -ge 5 ] && [ ${#SR_SESSION_ID} -le 20 ] \
    || die "SR_SESSION_ID 需 5-20 位，当前 ${#SR_SESSION_ID} 位：$SR_SESSION_ID"

if [ -z "$SR_KEY" ]; then
    if command -v openssl >/dev/null 2>&1; then
        SR_KEY=$(openssl rand -hex 24)
    else
        SR_KEY=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
    fi
    SR_KEY_GENERATED=1
else
    SR_KEY_GENERATED=0
fi

# ── 4. 配置文件 ────────────────────────────────────────────────────────
install -d -m 0755 "$SR_CONF_DIR"
if [ -f "$SR_CONF" ] && [ "$FORCE" != 1 ]; then
    log "保留已有配置：$SR_CONF（要重写加 --force）"
    # 回读生效值，否则下面渲染的 Description 和结尾提示会与实际配置不一致
    cfg() { sed -n "s|^$1=||p" "$SR_CONF" | head -1; }
    [ -n "$(cfg SR_SESSION_ID)" ] && SR_SESSION_ID=$(cfg SR_SESSION_ID)
    [ -n "$(cfg SR_KEY)" ] && SR_KEY=$(cfg SR_KEY)
    [ -n "$(cfg SR_RELAY_URL)" ] && SR_RELAY_URL=$(cfg SR_RELAY_URL)
    SR_KEY_GENERATED=0
else
    log "写入配置：$SR_CONF"
    sed -e "s|__SR_SESSION_ID__|$SR_SESSION_ID|g" \
        -e "s|__SR_KEY__|$SR_KEY|g" \
        -e "s|__SR_ROOT__|$SR_ROOT|g" \
        -e "s|^SR_RELAY_URL=.*|SR_RELAY_URL=$SR_RELAY_URL|" \
        -e "s|^SR_SHELL=.*|SR_SHELL=$SR_SHELL|" \
        "$TPL/agent.env" > "$SR_CONF"
    chown root:root "$SR_CONF"
    chmod 0600 "$SR_CONF"
fi

# ── 5. systemd unit ────────────────────────────────────────────────────
log "写入 unit：$SR_UNIT"
SR_DESC="shell-remote agent (${SR_SESSION_ID})"
sed -e "s|__SR_USER__|$SR_USER|g" \
    -e "s|__SR_GROUP__|$SR_USER|g" \
    -e "s|__SR_BIN__|$SR_BIN|g" \
    -e "s|__SR_CONF__|$SR_CONF|g" \
    -e "s|__SR_SERVICE__|$SR_SERVICE_NAME|g" \
    -e "s|__SR_DESC__|$SR_DESC|g" \
    "$TPL/shell-remote-agent.service" > "$SR_UNIT"
chmod 0644 "$SR_UNIT"

systemctl daemon-reload
systemctl enable "$SR_SERVICE_NAME" >/dev/null
systemctl restart "$SR_SERVICE_NAME"

# ── 6. 结果 ────────────────────────────────────────────────────────────
sleep 2
echo
log "服务状态："
systemctl is-enabled "$SR_SERVICE_NAME" | sed 's/^/  开机自启: /'
systemctl is-active  "$SR_SERVICE_NAME" | sed 's/^/  当前状态: /'
echo
log "最近日志："
journalctl -u "$SR_SERVICE_NAME" -n 12 --no-pager -o cat | sed 's/^/  /'
echo
log "运行账户 : $SR_USER  (home=$SR_HOME, root=$SR_ROOT)"
log "会话 ID  : $SR_SESSION_ID"
if [ "$SR_KEY_GENERATED" = 1 ]; then
    log "鉴权 key : $SR_KEY   ← 随机生成，请立即保存；之后只能从 $SR_CONF 读取"
fi
log "relay    : $SR_RELAY_URL"
log "浏览器访问: $SR_RELAY_URL/  →  会话 $SR_SESSION_ID → token=$SR_KEY"
