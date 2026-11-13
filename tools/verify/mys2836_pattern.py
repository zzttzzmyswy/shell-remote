#!/usr/bin/env python3
"""MYS-2836 运行时验证用的确定性测试画面（X11）。

画一块**有明确色度**、且每秒都在小范围运动的窗口：
  - 有强烈色度 → 灰度开关必须让它变成无色度（可用像素饱和度断言）；
  - 每秒都在动 → 编码器每拍都产真实新帧，帧计数才有意义。

低熵是刻意的：整屏高熵条纹会让 1080p 软件 AV1 掉到个位数 fps，把编码耗时
和被测的灰度切换逻辑混在一起。

用法： python3 mys2836_pattern.py [W] [H]     （需要 $DISPLAY）
"""
import os
import sys
import tkinter as tk

W = int(sys.argv[1]) if len(sys.argv) > 1 else 1280
H = int(sys.argv[2]) if len(sys.argv) > 2 else 720
READY = os.environ.get('MYS2836_READY_FILE')

root = tk.Tk()
root.geometry(f'{W}x{H}+0+0')
root.overrideredirect(True)
cv = tk.Canvas(root, width=W, height=H, highlightthickness=0, bd=0)
cv.pack()

# 静态背景：竖向色相渐变（只画一次）。
for i in range(0, H, 24):
    r = (i * 3) % 256
    g = (i * 7) % 256
    b = (i * 11) % 256
    cv.create_rectangle(0, i, W, i + 24, fill=f'#{r:02x}{g:02x}{b:02x}', width=0)

state = {'v': 0}
ids = {}


def frame():
    state['v'] += 1
    v = state['v']
    x = 40 + (v * 9) % max(1, W - 260)
    y = 40 + (v * 5) % max(1, H - 220)
    if 'box' not in ids:
        ids['box'] = cv.create_rectangle(x, y, x + 220, y + 180,
                                        fill='#ff0000', outline='#00ff00', width=6)
        ids['txt'] = cv.create_text(W // 2, H - 60, text='', fill='#ffffff',
                                    font=('DejaVu Sans', 32, 'bold'))
    else:
        cv.coords(ids['box'], x, y, x + 220, y + 180)
    cv.itemconfigure(ids['txt'], text=f'MYS-2836 pattern {v}')
    if READY and v == 3:
        # 前几帧画完再报就绪：验证脚本据此确认画面真的上了屏（不是空/黑）。
        try:
            open(READY, 'w').write(str(os.getpid()))
        except OSError:
            pass
    root.after(33, frame)   # ~30fps


frame()
root.mainloop()
