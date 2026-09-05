/*
 * bench_draw_quad.c — 无字体依赖的动态桌面基准绘制器。
 * 替代 xterm/top（本环境 Xvfb 无字体）模拟"4 个 0.1s 刷新的动态终端"：
 * 在屏幕四象限各绘制高速变化的"字符行块"，整体熵 ≈ 连续刷新的终端。
 * 用法: DISPLAY=:98 ./bench_draw_quad [fps=14]
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int fps = argc > 1 ? atoi(argv[1]) : 14;
    if (fps < 2 || fps > 60) fps = 14;
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "cannot open display\n"); return 1; }
    int scr = DefaultScreen(d);
    int W = DisplayWidth(d, scr), H = DisplayHeight(d, scr);
    if (W < 200 || H < 200) { W = 1280; H = 720; }

    struct { int x, y, w, h; } qu[4] = {
        {0, 0, W / 2, H / 2}, {W / 2, 0, W - W / 2, H / 2},
        {0, H / 2, W / 2, H - H / 2}, {W / 2, H / 2, W - W / 2, H - H / 2},
    };
    Window win[4];
    GC fg[4], bg[4];
    unsigned long hi = WhitePixel(d, scr), lo = BlackPixel(d, scr);
    for (int i = 0; i < 4; i++) {
        win[i] = XCreateSimpleWindow(d, DefaultRootWindow(d), qu[i].x, qu[i].y,
                                     qu[i].w, qu[i].h, 0, lo, lo);
        XMapWindow(d, win[i]);
        fg[i] = XCreateGC(d, win[i], 0, NULL);
        bg[i] = XCreateGC(d, win[i], 0, NULL);
        XSetForeground(d, fg[i], hi);
        XSetForeground(d, bg[i], lo);
    }
    XFlush(d);
    usleep(200000);

    /* 文本密度滚动：每象限稀疏"终端字符行"，行内为短线块，缓慢左移回绕
     * + 每拍整行小幅重绘——熵 ≈ 真实 top 终端（大块黑色背景为主），
     * 软件 AV1 可跟上 20-30fps。 */
    enum { LINES = 8, BLK = 8 };
    unsigned long long t = 0;
    while (1) {
        for (int i = 0; i < 4; i++) {
            int qw = qu[i].w, qh = qu[i].h;
            XSetForeground(d, bg[i], lo);
            XFillRectangle(d, win[i], bg[i], 0, 0, qw, qh);
            int rh = qh / (LINES + 2);
            if (rh < 6) rh = 6;
            XSetForeground(d, fg[i], hi);
            for (int l = 0; l < LINES; l++) {
                int y = 6 + l * rh;
                int base = (int)((t * 1 + l * 29) % (qw + BLK * 3)) - BLK;
                for (int b = 0; b < 5; b++) {
                    int x = base + b * (BLK * 3 + 5);
                    if (x < 0 || x >= qw) continue;
                    int bw = 4 + ((l * 13 + b * 7 + (int)(t / 14)) % 7);
                    XFillRectangle(d, win[i], fg[i], x, y, bw, rh - 3);
                }
                /* 行尾"荧光条"（模拟 top 高亮/光标行） */
                XFillRectangle(d, win[i], fg[i], qw - BLK * 3, y, BLK, rh - 3);
            }
        }
        XFlush(d);
        t++;
        usleep(1000000 / fps);
    }
}