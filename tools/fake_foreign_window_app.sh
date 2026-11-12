#!/bin/sh
# 负例：**别人的窗口**在屏幕上，但本进程不创建窗口。
#
# 用途：证明门禁按 `_NET_WM_PID` 判归属，而不是"屏幕上有没有窗口"或"标题像不像"。
# 上一版门禁的失败模式之一就是拿窗口标题/名字去找，同机的另一个 GUI 会被算成
# 自己开窗成功。本程序刻意让一个**子进程**（不是自己）弹出一个标题含
# "shell-remote" 的窗口，然后自己一直不建窗口。
#
# 子进程也由本脚本记录并回收，绝不留残留。
set -u

python3 - <<'PY' &
import tkinter as tk
r = tk.Tk()
# 标题刻意模仿被测程序：按标题匹配的门禁会把它当成自己开窗。
r.title("shell-remote · 连接设置")
r.geometry("720x660+120+120")
r.after(60000, r.destroy)
r.mainloop()
PY
child=$!

# 本进程自己不建窗口，但一直活着——门禁如果不看 PID 归属就会等到子进程的窗口。
sleep 60
kill -9 "$child" 2>/dev/null
wait "$child" 2>/dev/null
exit 0
