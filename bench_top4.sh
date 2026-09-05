#!/bin/bash
# 用户指定的标准动态基准画面：桌面放置 4 个 0.1s 刷新的 top 终端。
# 用法: DISPLAY=:98 bash bench_top4.sh
set -u
DISPLAY="${DISPLAY:-:98}"
export DISPLAY

spawn() { # $1=x $2=y $3=title
  local x=$1 y=$2 title=$3
  xterm -T "$title" -geometry 96x28+${x}+${y} -e bash -c 'top -b -d 0.1 -n 0' 2>/dev/null &
}
spawn 5   5   top1
spawn 660 5   top2
spawn 5   430 top3
spawn 660 430 top4
echo "4 个 top 终端已启动（0.1s 刷新，约 10 行/s 滚动 + 高频整屏变化）"
echo "kill 用: pkill -f 'top -b -d 0.1' ; pkill -f 'xterm -T top'"