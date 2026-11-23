"""A desktop component that grabs keys on the root window, as a media-keys
handler does: `python3 key_grabber.py <log> <keysym>...` grabs each key with
any modifiers and appends one line per key event it receives to the log,
`<press|release> <keysym> <state>`. It prints `ready` once the grabs are in.
Plain Xlib through ctypes, so it needs nothing beyond libX11."""

import ctypes
import ctypes.util
import sys

KEY_PRESS, KEY_RELEASE = 2, 3
ANY_MODIFIER = 1 << 15
GRAB_MODE_ASYNC = 1


class XKeyEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("root", ctypes.c_ulong),
        ("subwindow", ctypes.c_ulong),
        ("time", ctypes.c_ulong),
        ("x", ctypes.c_int),
        ("y", ctypes.c_int),
        ("x_root", ctypes.c_int),
        ("y_root", ctypes.c_int),
        ("state", ctypes.c_uint),
        ("keycode", ctypes.c_uint),
        ("same_screen", ctypes.c_int),
    ]


class XEvent(ctypes.Union):
    _fields_ = [("type", ctypes.c_int), ("xkey", XKeyEvent), ("pad", ctypes.c_long * 24)]


def main():
    log, names = sys.argv[1], sys.argv[2:]
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
    xlib.XOpenDisplay.restype = ctypes.c_void_p
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XDefaultRootWindow.restype = ctypes.c_ulong
    xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    xlib.XStringToKeysym.restype = ctypes.c_ulong
    xlib.XStringToKeysym.argtypes = [ctypes.c_char_p]
    xlib.XKeysymToKeycode.restype = ctypes.c_ubyte
    xlib.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    xlib.XGrabKey.argtypes = [
        ctypes.c_void_p, ctypes.c_int, ctypes.c_uint, ctypes.c_ulong,
        ctypes.c_int, ctypes.c_int, ctypes.c_int,
    ]
    xlib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    xlib.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.POINTER(XEvent)]
    display = xlib.XOpenDisplay(None)
    if not display:
        raise SystemExit("cannot open the display")
    root = xlib.XDefaultRootWindow(display)
    names_by_code = {}
    for name in names:
        code = xlib.XKeysymToKeycode(display, xlib.XStringToKeysym(name.encode()))
        if not code:
            raise SystemExit(f"no key code for {name}")
        names_by_code[code] = name
        xlib.XGrabKey(display, code, ANY_MODIFIER, root, 1, GRAB_MODE_ASYNC, GRAB_MODE_ASYNC)
    xlib.XSync(display, 0)
    print("ready", flush=True)
    event = XEvent()
    while True:
        xlib.XNextEvent(display, ctypes.byref(event))
        if event.type not in (KEY_PRESS, KEY_RELEASE):
            continue
        kind = "press" if event.type == KEY_PRESS else "release"
        name = names_by_code.get(event.xkey.keycode, str(event.xkey.keycode))
        with open(log, "a") as stream:
            stream.write(f"{kind} {name} {event.xkey.state & 0xff}\n")


if __name__ == "__main__":
    main()
