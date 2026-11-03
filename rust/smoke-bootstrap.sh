#!/bin/bash
set -euo pipefail

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${ILIA_SMOKE_DBUS:-}" ]]; then
    exec dbus-run-session -- env ILIA_SMOKE_DBUS=1 "$0" "$@"
fi

root=$(cd "$(dirname "$0")/.." && pwd)
binary="$root/rust/target/release/ilia"
tmp=$(mktemp -d)
trap 'kill "${xvfb_pid:-}" 2>/dev/null || true; rm -rf "$tmp"' EXIT
Xvfb -displayfd 3 -screen 0 1024x768x24 -ac -noreset -nolisten tcp \
    3>"$tmp/display" >"$tmp/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in $(seq 1 100); do
    [[ -s "$tmp/display" ]] && break
    kill -0 "$xvfb_pid" 2>/dev/null || {
        cat "$tmp/xvfb.log" >&2
        exit 1
    }
    sleep .05
done
[[ -s "$tmp/display" ]]
read -r display_number <"$tmp/display"
display=":$display_number"
export DISPLAY=$display GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 NO_AT_BRIDGE=1
export GSETTINGS_BACKEND=memory HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config"
export XDG_DATA_HOME="$tmp/data" XDG_CACHE_HOME="$tmp/cache" XDG_RUNTIME_DIR="$tmp/runtime"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
mkdir -p "$tmp/schemas"
cp "$root"/data/*.gschema.xml "$tmp/schemas/"
glib-compile-schemas "$tmp/schemas"
export GSETTINGS_SCHEMA_DIR="$tmp/schemas"
unset SWAYSOCK I3SOCK

for invocation in "" "-p apps" "-p terminal" "-p notifications" "-p keybindings" "-p textlist" "-p windows" "-p tracker" "-a" "-a -p windows"; do
    : >"$tmp/out"; : >"$tmp/err"
    if [[ "$invocation" == "-p textlist" ]]; then
        printf 'alpha\nbeta\n' | "$binary" $invocation >"$tmp/out" 2>"$tmp/err" &
    else
        "$binary" $invocation >"$tmp/out" 2>"$tmp/err" &
    fi
    pid=$!
    window=
    for _ in $(seq 1 100); do
        window=$(xdotool search --onlyvisible --class ilia 2>/dev/null | head -1 || true)
        [[ -n "$window" ]] && break
        sleep .05
    done
    [[ -n "$window" ]]
    eval "$(xdotool getwindowgeometry --shell "$window")"
    (( WIDTH >= 100 && HEIGHT >= 100 ))
    xdotool windowfocus "$window"
    xdotool key --clearmodifiers Escape
    for _ in $(seq 1 100); do
        ! kill -0 "$pid" 2>/dev/null && break
        sleep .05
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
    wait "$pid"
    if [[ -s "$tmp/out" || -s "$tmp/err" ]]; then
        printf 'unexpected output for invocation: %s\n' "${invocation:-<default>}" >&2
        sed 's/^/stdout: /' "$tmp/out" >&2
        sed 's/^/stderr: /' "$tmp/err" >&2
        exit 1
    fi
done

invocation="-p textlist"
: >"$tmp/out"; : >"$tmp/err"
"$binary" $invocation </dev/null >"$tmp/out" 2>"$tmp/err" &
pid=$!
window=
for _ in $(seq 1 100); do
    window=$(xdotool search --onlyvisible --class ilia 2>/dev/null | head -1 || true)
    [[ -n "$window" ]] && break
    sleep .05
done
[[ -n "$window" ]]
xdotool windowfocus "$window"
xdotool key --clearmodifiers Escape
for _ in $(seq 1 100); do
    ! kill -0 "$pid" 2>/dev/null && break
    sleep .05
done
if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    exit 1
fi
wait "$pid"
[[ ! -s "$tmp/out" && ! -s "$tmp/err" ]]

if "$binary" -t "$tmp/missing.css" >"$tmp/out" 2>"$tmp/err"; then exit 1; fi
[[ ! -s "$tmp/out" ]]
[[ "$(cat "$tmp/err")" == "File '$tmp/missing.css' does not exist." ]]
