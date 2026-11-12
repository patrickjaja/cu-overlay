#!/usr/bin/env bash
# End-to-end test of the x11 backend against a throwaway Xvfb (no window on
# the host). Xvfb is not XWayland and has no compositing manager, so the
# backend is forced (CU_OVERLAY_BACKEND=x11); what the real target (Mutter's
# XWayland) adds is checked live on GNOME, see DESIGN.md.
#
# Checked here: the backend refuses a plain X server in auto mode; `ready`;
# the four windows (override-redirect, 32-bit, names, pid, window type,
# geometry from the logical layout, stacking); the pointer passes through
# them (the window under the pointer is the root); `_NET_WM_WINDOW_OPACITY`
# on capture hide/restore for both scopes; the factor from Xft.dpi; a stale
# RandR corrected by _NET_DESKTOP_GEOMETRY and by _GTK_WORKAREAS_D0 (Mutter's
# properties, set by hand); an unplug and replug of the shown output; nothing
# left after `hide` and stdin EOF. With picom and ImageMagick's `import`
# installed, also pixels: rim, arrow, transparency, both capture scopes and
# the badge, composited by picom (xrender backend).
#
# Usage: scripts/test-xvfb.sh
# Env:
#   CU_OVERLAY_DEMO  demo binary built with the x11 feature (default: the
#                    x86_64 musl release build, then target/release)
#   XVFB             Xvfb binary (default: Xvfb on PATH)
#   EVIDENCE_DIR     keep screenshots here (default: a temp dir, removed)
#   XVFB_PIXELS=0    skip the pixel checks even when picom is installed
# Requires: Xvfb, xprop, xwininfo, xrandr, xrdb, xdotool, python3 with PIL.
# Optional: picom, import (ImageMagick) for the pixel checks.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
TGT="${CARGO_TARGET_DIR:-$ROOT/target}"
DEMO="${CU_OVERLAY_DEMO:-}"
for cand in "$TGT/x86_64-unknown-linux-musl/release/cu-overlay-demo" "$TGT/release/cu-overlay-demo" "$TGT/debug/cu-overlay-demo"; do
  [ -z "$DEMO" ] && [ -x "$cand" ] && DEMO="$cand"
done
[ -x "${DEMO:-}" ] || { echo "error: cu-overlay-demo not built (cargo build --release --features x11)" >&2; exit 1; }
XVFB="${XVFB:-$(command -v Xvfb || true)}"
[ -x "${XVFB:-}" ] || { echo "error: Xvfb not installed (xvfb)" >&2; exit 1; }
for t in xprop xwininfo xrandr xrdb xdotool python3; do
  command -v "$t" >/dev/null || { echo "error: $t not installed" >&2; exit 1; }
done
python3 -c 'import PIL' 2>/dev/null || { echo "error: python3 PIL missing (python3-pil)" >&2; exit 1; }
PIXELS="${XVFB_PIXELS:-1}"
command -v picom >/dev/null && command -v import >/dev/null || PIXELS=0
echo ">>> demo: $DEMO"
echo ">>> Xvfb: $XVFB ($("$XVFB" -version 2>&1 | grep -m1 -i 'x.org x server' || echo '?'))"
[ "$PIXELS" = 1 ] && echo ">>> pixel checks with picom $(picom --version 2>/dev/null | head -1)" \
  || echo ">>> pixel checks skipped (picom or import missing, or XVFB_PIXELS=0)"

RUN="$(mktemp -d /tmp/cuxv.XXXX)"
EVID="${EVIDENCE_DIR:-$RUN/evidence}"
mkdir -p "$EVID"
unset WAYLAND_DISPLAY
PIDS=()
cleanup() {
  exec 3>&- 2>/dev/null || true
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "${RUN:?}"
}
trap cleanup EXIT

D=99
while [ -e "/tmp/.X11-unix/X$D" ] || [ -e "/tmp/.X$D-lock" ]; do D=$((D + 1)); done
"$XVFB" ":$D" -screen 0 1280x800x24 -nolisten tcp >"$RUN/xvfb.log" 2>&1 &
PIDS+=("$!")
export DISPLAY=":$D"
for _ in $(seq 1 50); do xprop -root >/dev/null 2>&1 && break; sleep 0.1; done
xprop -root >/dev/null 2>&1 || { echo "error: Xvfb did not start" >&2; cat "$RUN/xvfb.log" >&2; exit 1; }
if [ "$PIXELS" = 1 ]; then
  picom --backend xrender --no-fading-openclose --config /dev/null >"$RUN/picom.log" 2>&1 &
  PIDS+=("$!")
  sleep 0.5
fi

fail=0
pass=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*" >&2; fail=$((fail + 1)); }
check() { # check <description> <command...>
  local d="$1"; shift
  if "$@"; then ok "$d"; else bad "$d"; fi
}

OUT="" ; OPID=""
start_overlay() { # $1 = tag, rest = env assignments
  local tag="$1"; shift
  mkfifo "$RUN/$tag.fifo"
  env CU_OVERLAY_BACKEND=x11 "$@" "$DEMO" <"$RUN/$tag.fifo" >"$RUN/$tag.out" 2>"$RUN/$tag.err" &
  OPID=$!
  PIDS+=("$OPID")
  exec 3>"$RUN/$tag.fifo"
  OUT="$RUN/$tag.out"
  SEEN=0
}
send() { printf '%s\n' "$1" >&3; }
SEEN=0
wait_event() { # wait_event <regex> [timeout s]: a NEW matching line since the last call
  local re="$1" t="${2:-3}" end=$((SECONDS + ${2:-3})) n line
  while :; do
    n=0
    while IFS= read -r line; do
      n=$((n + 1))
      if [ "$n" -gt "$SEEN" ] && [[ "$line" =~ $re ]]; then SEEN=$n; LAST="$line"; return 0; fi
    done <"$OUT"
    [ "$SECONDS" -ge "$end" ] && { echo "    (timeout ${t}s waiting for $re; output:)" >&2; cat "$OUT" >&2; return 1; }
    sleep 0.05
  done
}
# the overlay's windows, bottom to top (xwininfo lists children top first)
wins() { xwininfo -root -children | awk '/"claude-cu-overlay"/ {print $1}' | tac; }
geom() { # geom <win> -> "x y w h"
  xwininfo -id "$1" | awk '/Absolute upper-left X/ {x=$4} /Absolute upper-left Y/ {y=$4} /Width:/ {w=$2} /Height:/ {h=$2} END {print x, y, w, h}'
}
opacity() { xprop -id "$1" _NET_WM_WINDOW_OPACITY | awk -F' = ' '{print $2}'; }
opacities() { local o=""; for w in $(wins); do o="$o $(opacity "$w")"; done; echo "${o# }"; }
under_pointer() { # under_pointer <x> <y> -> window id (decimal) under the pointer
  xdotool mousemove "$1" "$2" >/dev/null
  xdotool getmouselocation --shell | awk -F= '/^WINDOW=/ {print $2}'
}
shot() { import -window root "$EVID/$1.png"; }
px() { # px <shot> <python expression over p(x,y)>
  python3 -I - "$EVID/$1.png" "$2" <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
p = lambda x, y: im.getpixel((x, y))
clay = lambda c: c[0] > 150 and 70 < c[1] < 160 and c[2] < 130 and c[0] - c[2] > 70
white = lambda c: min(c) > 200
black = lambda c: max(c) < 12
region = lambda x0, y0, x1, y1, f: sum(1 for y in range(y0, y1) for x in range(x0, x1) if f(p(x, y)))
sys.exit(0 if eval(sys.argv[2]) else 1)
PY
}
MAXOP=4294967295

echo "--- selection"
set +e
"$DEMO" </dev/null >"$RUN/auto.out" 2>&1
code=$?
set -e
check "auto mode refuses a plain X server (exit 2, names XWayland)" \
  test "$code" = 2 -a -n "$(grep -o 'not XWayland' "$RUN/auto.out")"

echo "--- windows"
start_overlay main
wait_event '"event":"ready"'
check "ready: backend x11, alphaModifier, badge, outputs [screen 1280x800 scale 1]" \
  test -n "$(grep -F '"backend":"x11","alphaModifier":true,"fractionalScale":false,"badge":true' <<<"$LAST" | grep -F '"outputs":[{"name":"screen","x":0,"y":0,"width":1280,"height":800,"scale":1.0}]')"
send '{"cmd":"show","output":"screen","look":"rim","arrow":{"x":300,"y":200}}'
wait_event '"event":"shown","output":"screen","scale":1.0'
ok "shown"
sleep 0.3
mapfile -t W < <(wins)
check "four windows named claude-cu-overlay" test "${#W[@]}" = 4
MAIN=${W[0]}; BADGE=${W[1]}; ARROW=${W[3]}
check "main window covers the output: 0 0 1280 800" test "$(geom "$MAIN")" = "0 0 1280 800"
check "arrow window: 192 px box with the hotspot at (300,200)" test "$(geom "$ARROW")" = "236 136 192 192"
all_or=1; all_32=1
for w in "${W[@]}"; do
  xwininfo -all -id "$w" | grep -q 'Override Redirect State: yes' || all_or=0
  xwininfo -id "$w" | grep -q 'Depth: 32' || all_32=0
done
check "override-redirect, depth 32" test "$all_or$all_32" = 11
props="$(xprop -id "$MAIN" WM_CLASS _NET_WM_NAME _NET_WM_PID _NET_WM_WINDOW_TYPE _NET_WM_BYPASS_COMPOSITOR)"
check "WM_CLASS / _NET_WM_NAME claude-cu-overlay" test -n "$(grep -F 'WM_CLASS(STRING) = "claude-cu-overlay", "claude-cu-overlay"' <<<"$props" | head -1)" -a -n "$(grep -F '_NET_WM_NAME(UTF8_STRING) = "claude-cu-overlay"' <<<"$props")"
check "_NET_WM_PID is the overlay's pid" test -n "$(grep -F "_NET_WM_PID(CARDINAL) = $OPID" <<<"$props")"
check "window type NOTIFICATION, never unredirected" test -n "$(grep -F '_NET_WM_WINDOW_TYPE_NOTIFICATION' <<<"$props")" -a -n "$(grep -F '_NET_WM_BYPASS_COMPOSITOR(CARDINAL) = 2' <<<"$props")"
check "opacity: main and arrow opaque, badge and ring 0" test "$(opacities)" = "$MAXOP 0 0 $MAXOP"
root="$(xwininfo -root | awk '/Window id:/ {print $4}')"
check "pointer on the arrow's hotspot is over the root (empty input shape)" test "$(printf '0x%x' "$(under_pointer 300 200)")" = "$root"
check "pointer on the rim is over the root" test "$(printf '0x%x' "$(under_pointer 0 400)")" = "$root"
if [ "$PIXELS" = 1 ]; then
  shot rim
  check "pixels: rim on all four edges, transparent inside" px rim 'all(clay(p(*q)) for q in [(0,400),(1,400),(1279,400),(640,0),(640,799)]) and black(p(640,400))'
  check "pixels: arrow at (300,200), white outline at the tip, clay body" px rim 'clay(p(312,212)) and region(296,195,304,203,white) > 4 and black(p(290,190))'
fi

echo "--- capture"
send '{"cmd":"capture","phase":"hide","id":1,"scope":"all"}'
wait_event '"event":"ack","id":1'
check "scope all: every window at opacity 0 by the ack" test "$(opacities)" = "0 0 0 0"
[ "$PIXELS" = 1 ] && { shot cap_all; check "pixels: nothing drawn during an all hide" px cap_all 'region(0,0,1280,800,lambda c: not black(c)) == 0'; }
send '{"cmd":"capture","phase":"restore","id":2}'
wait_event '"event":"ack","id":2'
check "restore: main and arrow back" test "$(opacities)" = "$MAXOP 0 0 $MAXOP"
send '{"cmd":"press","x":300,"y":200}'
sleep 0.1
send '{"cmd":"capture","phase":"hide","id":3,"scope":"cursor"}'
wait_event '"event":"ack","id":3'
check "scope cursor: rim kept, ring and arrow at 0" test "$(opacities)" = "$MAXOP 0 0 0"
[ "$PIXELS" = 1 ] && { shot cap_cursor; check "pixels: cursor hide keeps the rim, no arrow or ring" px cap_cursor 'clay(p(0,400)) and region(200,100,420,320,lambda c: not black(c)) == 0'; }
send '{"cmd":"capture","phase":"restore","id":4}'
wait_event '"event":"ack","id":4'
check "restore: arrow back, ring back while its animation runs" test "$(opacities)" = "$MAXOP 0 $MAXOP $MAXOP"

echo "--- badge and glow"
send '{"cmd":"show","output":"screen","look":"glow","badge":true}'
sleep 1.5
check "badge window visible, centred box" test "$(opacity "$BADGE")" = "$MAXOP"
if [ "$PIXELS" = 1 ]; then
  shot badge
  check "pixels: the badge pill at the centre (near-white) with clay text" px badge 'region(480,380,800,420,white) > 3000 and region(500,385,790,415,clay) > 150'
  check "pixels: glow is translucent clay at the edge" px badge 'not clay(p(0,400)) and p(0,400)[0] > 60 and p(0,400)[0] > p(0,400)[2] + 20 and black(p(640,200))'
fi

echo "--- scale and stale RandR"
send '{"cmd":"hide"}'
wait_event '"event":"hidden"'
check "hide: no overlay window left" test -z "$(wins)"
printf 'Xft.dpi:\t192\n' | xrdb -nocpp -merge
wait_event '"event":"outputs".*"width":640,"height":400,"scale":2.0' 3
ok "Xft.dpi 192: outputs event, the screen is 640x400 logical at factor 2"
send '{"cmd":"show","output":"screen","look":"rim","arrow":{"x":150,"y":100}}'
wait_event '"event":"shown","output":"screen","scale":2.0'
sleep 0.3
mapfile -t W < <(wins)
check "factor 2: main 1280x800 X px, arrow box 384 px at (150-64)*2" \
  test "$(geom "${W[0]}") / $(geom "${W[3]}")" = "0 0 1280 800 / 172 72 384 384"
[ "$PIXELS" = 1 ] && { shot scale2; check "pixels: arrow drawn at factor 2 (body below-right of (300,200) X px)" px scale2 'clay(p(324,224)) and region(292,190,308,206,white) > 8'; }
# Mutter after a live switch to native scaling: the desktop at factor 2,
# RandR (the 1280x800 root) still at factor 1
xprop -root -f _NET_DESKTOP_GEOMETRY 32c -set _NET_DESKTOP_GEOMETRY 2560,1600
wait_event '"event":"outputs".*"width":1280,"height":800,"scale":2.0' 3
ok "stale RandR (desktop 2560x1600 over a 1280x800 root): corrected to 1280x800 logical"
check "stderr names the correction" grep -q 'scaled by 2/1' "$RUN/main.err"
sleep 0.3
check "windows follow: main at the corrected 2560x1600" test "$(geom "${W[0]}")" = "0 0 2560 1600"
xprop -root -f _GTK_WORKAREAS_D0 32c -set _GTK_WORKAREAS_D0 132,58,2428,1542
sleep 0.6
check "with Mutter's work area the fit is used (same layout, no outputs event)" grep -q "fitted to Mutter's work areas" "$RUN/main.err"
check "no outputs event for an unchanged layout" test "$(grep -c '"event":"outputs"' "$OUT")" = 2
xprop -root -remove _GTK_WORKAREAS_D0
xprop -root -remove _NET_DESKTOP_GEOMETRY
printf 'Xft.dpi:\t96\n' | xrdb -nocpp -merge
wait_event '"event":"outputs".*"width":1280,"height":800,"scale":1.0' 3
ok "back to factor 1"

echo "--- unplug and replug"
# switching Xvfb's only output off and on again unplugs and replugs the
# shown output (xrandr reports an error for the screen size; the CRTC is off)
xrandr --output screen --off >/dev/null 2>&1 || true
wait_event '"event":"outputs","outputs":\[\]' 3 && ok "unplug: outputs event without the output"
wait_event '"event":"error","message":"the output of the overlay was removed","fatal":false' && ok "unplug while shown: non-fatal error"
wait_event '"event":"hidden"' && ok "unplug while shown: hidden"
check "unplug: windows gone, process alive" test -z "$(wins)" -a -d "/proc/$OPID"
xrandr --output screen --auto >/dev/null 2>&1 || true
wait_event '"event":"outputs".*"name":"screen".*"width":1280,"height":800' 3 && ok "replug: outputs event with the output"
send '{"cmd":"show","output":"screen","look":"rim"}'
wait_event '"event":"shown","output":"screen"' && ok "show again after the replug"

echo "--- lifetime"
exec 3>&-
set +e
for _ in $(seq 1 40); do kill -0 "$OPID" 2>/dev/null || break; sleep 0.05; done
wait "$OPID"
code=$?
set -e
check "stdin EOF: exit 0" test "$code" = 0
check "nothing left on the screen" test -z "$(wins)"

echo
echo "passed $pass, failed $fail"
[ "$fail" = 0 ]
