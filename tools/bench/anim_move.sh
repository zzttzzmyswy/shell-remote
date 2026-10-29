#!/bin/bash
# 制造高熵"移动窗口"场景: 内容滚动的 xterm + 快速几何移动
export DISPLAY=:98
xterm -T animwin -geometry 640x360+10+10 -e bash -c \
  'i=0; while true; do for c in $(seq 1 25); do echo "frame $i seg $c 0123456789 abcdefghijklmnopqrstuvwxyz ABCD"; done; i=$((i+1)); done' 2>/dev/null &
XPID=$!
sleep 1.5
WIN=$(xdotool search --name animwin 2>/dev/null | head -1)
echo "anim xterm pid=$XPID window=$WIN"
if [ -z "$WIN" ]; then echo "NO anim window found"; kill $XPID; exit 1; fi
while true; do
  for i in $(seq 1 200); do
    x=$(( (i * 41) % 500 ))
    y=$(( (i * 71) % 200 ))
    xdotool windowmove "$WIN" $x $y 2>/dev/null
    sleep 0.02
  done
done