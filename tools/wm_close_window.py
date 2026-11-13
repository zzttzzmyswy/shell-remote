#!/usr/bin/env python3
"""向窗口发送 WM_DELETE_WINDOW（等价于点关闭按钮），而不是 XDestroyWindow。

xdotool windowclose 发的是 XDestroyWindow：服务端直接销毁窗口，客户端（winit）
随后访问已失效的窗口 ID 会 panic（实测 TranslateCoordinates BadWindow → rc=101）。
真实用户点关闭走的是 WM_DELETE_WINDOW 协议消息。
用法: wmclose.py <window-id>
"""
import ctypes, ctypes.util, sys

win = int(sys.argv[1], 0)
xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
xlib.XOpenDisplay.restype = ctypes.c_void_p
xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
xlib.XInternAtom.restype = ctypes.c_ulong
xlib.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
xlib.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                            ctypes.c_long, ctypes.c_void_p]
xlib.XFlush.argtypes = [ctypes.c_void_p]
xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
xlib.XDefaultRootWindow.restype = ctypes.c_ulong
xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]

dpy = xlib.XOpenDisplay(None)
if not dpy:
    print("cannot open display", file=sys.stderr); sys.exit(2)

WM_PROTOCOLS = xlib.XInternAtom(dpy, b"WM_PROTOCOLS", 0)
WM_DELETE = xlib.XInternAtom(dpy, b"WM_DELETE_WINDOW", 0)

# typedef struct { int type; unsigned long serial; Bool send_event; Display *display;
#   Window window; Atom message_type; int format; union { char b[20]; short s[10];
#   long l[5]; } data; } XClientMessageEvent;
class ClientMessageData(ctypes.Union):
    _fields_ = [("b", ctypes.c_char * 20), ("s", ctypes.c_short * 10),
                ("l", ctypes.c_long * 5)]

class XClientMessageEvent(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong),
                ("send_event", ctypes.c_int), ("display", ctypes.c_void_p),
                ("window", ctypes.c_ulong), ("message_type", ctypes.c_ulong),
                ("format", ctypes.c_int), ("data", ClientMessageData)]

class XEvent(ctypes.Union):
    _fields_ = [("type", ctypes.c_int), ("xclient", XClientMessageEvent),
                ("pad", ctypes.c_long * 24)]

ev = XEvent()
ev.xclient.type = 33          # ClientMessage
ev.xclient.serial = 0
ev.xclient.send_event = 1
ev.xclient.display = dpy
ev.xclient.window = win
ev.xclient.message_type = WM_PROTOCOLS
ev.xclient.format = 32
ev.xclient.data.l[0] = WM_DELETE
ev.xclient.data.l[1] = 0

NoEventMask = 0
xlib.XSendEvent(dpy, win, 0, NoEventMask, ctypes.byref(ev))
xlib.XFlush(dpy)
xlib.XCloseDisplay(dpy)
print("sent WM_DELETE_WINDOW to", win)
