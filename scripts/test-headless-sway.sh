#!/usr/bin/env bash
# End-to-end test of the layer-shell overlay against a throwaway HEADLESS
# sway (no window on the host, no real input devices). Drives
# cu-overlay-demo over its JSON-lines protocol and asserts on screenshots
# taken with wlroots-bridge.
#
# Usage:
#   scripts/test-headless-sway.sh            # functional assertions
#   scripts/test-headless-sway.sh --cpu      # also measure overlay CPU (3 x 10 s)
#
# Env:
#   CU_OVERLAY_DEMO     demo binary (default: target/x86_64-unknown-linux-musl/release/cu-overlay-demo,
#                       then target/release/cu-overlay-demo under $CARGO_TARGET_DIR or ./target)
#   WLROOTS_BRIDGE_BIN  screenshot tool (default: ../wlroots-bridge build, then
#                       /usr/lib/claude-desktop/resources/wlroots-bridge)
#   EVIDENCE_DIR        keep screenshots here (default: a temp dir, removed)
#
# Requires: sway, python3 with PIL (python3-pil), wlroots-bridge; foot for
# the click-through check (skipped with a notice when missing).
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
CPU=0
[ "${1:-}" = "--cpu" ] && CPU=1

TGT="${CARGO_TARGET_DIR:-$ROOT/target}"
DEMO="${CU_OVERLAY_DEMO:-}"
for cand in "$TGT/x86_64-unknown-linux-musl/release/cu-overlay-demo" "$TGT/release/cu-overlay-demo" "$TGT/debug/cu-overlay-demo"; do
  [ -z "$DEMO" ] && [ -x "$cand" ] && DEMO="$cand"
done
[ -x "${DEMO:-}" ] || { echo "error: cu-overlay-demo not built (cargo build --release)" >&2; exit 1; }
BRIDGE="${WLROOTS_BRIDGE_BIN:-}"
for cand in "$ROOT/../wlroots-bridge/target/x86_64-unknown-linux-musl/release/wlroots-bridge" \
            "$ROOT/../wlroots-bridge/target/release/wlroots-bridge" \
            /usr/lib/claude-desktop/resources/wlroots-bridge; do
  [ -z "$BRIDGE" ] && [ -x "$cand" ] && BRIDGE="$cand"
done
[ -x "${BRIDGE:-}" ] || { echo "error: wlroots-bridge not found (set WLROOTS_BRIDGE_BIN)" >&2; exit 1; }
command -v sway >/dev/null || { echo "error: sway not installed" >&2; exit 1; }
python3 -c 'import PIL' 2>/dev/null || { echo "error: python3 PIL missing (python3-pil)" >&2; exit 1; }
echo ">>> demo:   $DEMO"
echo ">>> bridge: $BRIDGE"

# Short socket path: XDG_RUNTIME_DIR is the Wayland socket's directory.
RUN="$(mktemp -d /tmp/xx.XXXX)"
chmod 700 "$RUN"
EVID="${EVIDENCE_DIR:-$RUN/evidence}"
mkdir -p "$EVID"
export XDG_RUNTIME_DIR="$RUN"
unset WAYLAND_DISPLAY DISPLAY
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
export SWAYSOCK="$RUN/sway.sock"
cat >"$RUN/sway.conf" <<'EOF'
xwayland disable
output HEADLESS-1 resolution 1280x800 position 0 0
default_border none
focus_follows_mouse no
EOF

PIDS=()
cleanup() {
  exec 3>&- 2>/dev/null || true
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  [ -n "${SWAY_PID:-}" ] && kill "$SWAY_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  # with EVIDENCE_DIR the screenshots live outside $RUN
  rm -rf "${RUN:?}"
  true
}
trap cleanup EXIT

sway -c "$RUN/sway.conf" >"$RUN/sway.log" 2>&1 &
SWAY_PID=$!
for _ in $(seq 1 50); do
  sock="$(find "$RUN" -maxdepth 1 -name 'wayland-*' -type s | head -1)"
  [ -n "$sock" ] && [ -S "$SWAYSOCK" ] && break
  sleep 0.1
done
[ -n "${sock:-}" ] || { echo "error: sway did not start" >&2; cat "$RUN/sway.log" >&2; exit 1; }
export WAYLAND_DISPLAY="$(basename "$sock")"
sleep 0.3
swaymsg -q create_output
swaymsg -q output HEADLESS-2 resolution 1920x1200 position 1280 0 scale 1.5
swaymsg -q focus output HEADLESS-1

fail=0
pass=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*" >&2; fail=$((fail + 1)); }

# --- overlay process helpers ---
OUT="" ; OPID=""
start_overlay() { # $1 = tag, rest = env assignments
  local tag="$1"; shift
  mkfifo "$RUN/$tag.fifo"
  env "$@" "$DEMO" <"$RUN/$tag.fifo" >"$RUN/$tag.out" 2>"$RUN/$tag.err" &
  OPID=$!
  PIDS+=("$OPID")
  exec 3>"$RUN/$tag.fifo"
  OUT="$RUN/$tag.out"
}
send() { printf '%s\n' "$1" >&3; }
# wait_event <regex> [timeout s]: waits for a NEW matching line since the last call
SEEN=0
wait_event() {
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
shot() { # shot <name> <output>
  "$BRIDGE" screenshot --display "$2" >"$RUN/$1.json"
  python3 -I - "$RUN/$1.json" "$EVID/$1.png" <<'PY'
import base64, io, json, sys
from PIL import Image
d = json.load(open(sys.argv[1]))
im = Image.open(io.BytesIO(base64.b64decode(d["base64"]))).convert("RGB")
im.save(sys.argv[2])
PY
}
# check <name> <output logical width> <python expressions over px(x,y) / region(...)>
check() {
  local name="$1" lw="$2" expr="$3"
  python3 -I - "$EVID/$name.png" "$lw" "$expr" <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
k = im.width / float(sys.argv[2])          # image px per logical px
def px(x, y):                               # logical -> pixel colour
    return im.getpixel((min(im.width - 1, int(x * k)), min(im.height - 1, int(y * k))))
def clay(p):   return p[0] > 150 and 70 < p[1] < 160 and p[2] < 130 and p[0] - p[2] > 70
def white(p):  return min(p) > 200
def black(p, t=24): return max(p) <= t
def reddish(p): return p[0] - p[2] > 25 and p[0] > 40
def region(x0, y0, x1, y1, pred):
    return sum(1 for y in range(int(y0 * k), int(y1 * k)) for x in range(int(x0 * k), int(x1 * k))
               if pred(im.getpixel((x, y))))
def all_black(t=24):
    return max(im.tobytes()) <= t
ok = eval(sys.argv[3])
sys.exit(0 if ok else 1)
PY
}
assert_shot() { # assert_shot <desc> <name> <lw> <expr>
  if check "$2" "$3" "$4"; then ok "$1 ($EVID/$2.png)"; else bad "$1 ($EVID/$2.png): $4"; fi
}

echo ">>> overlay A (all optional globals)"
start_overlay a
wait_event '"event":"ready"' && ok "ready: $LAST" || bad "no ready"
[[ "$LAST" == *'"alphaModifier":true'* && "$LAST" == *'"fractionalScale":true'* && "$LAST" == *'"badge":false'* ]] \
  && ok "ready reports alphaModifier, fractionalScale, badge:false" || bad "ready caps"
[[ "$LAST" == *'{"name":"HEADLESS-2","x":1280,"y":0,"width":1280,"height":800,"scale":1.5}'* ]] \
  && ok "ready lists HEADLESS-2 at 1280,0 1280x800 scale 1.5" || bad "ready outputs"

# rim + arrow on HEADLESS-1
send '{"cmd":"show","output":"HEADLESS-1","look":"rim","badge":true,"arrow":{"x":300,"y":200}}'
wait_event '"event":"shown","output":"HEADLESS-1","scale":1\.0' && ok "shown on HEADLESS-1" || bad "shown"
sleep 0.5
shot rim HEADLESS-1
assert_shot "rim on all four edges" rim 1280 'all(clay(px(*p)) for p in [(0,400),(1,400),(1279,400),(1278,400),(640,0),(640,1),(640,799),(640,798)]) and black(px(4,400)) and black(px(640,4))'
assert_shot "interior untouched" rim 1280 'black(px(640,400)) and black(px(900,600))'
assert_shot "arrow at (300,200): clay body down-right, white outline at the tip" rim 1280 'clay(px(312,212)) and region(296,195,304,203,white) > 4 and black(px(290,190)) and black(px(330,250))'

# glow look: bands at the edges, no arrow
send '{"cmd":"look","look":"glow"}'
sleep 0.6
shot glow HEADLESS-1
assert_shot "glow bands on the edges" glow 1280 'reddish(px(3,400)) and reddish(px(640,3)) and reddish(px(1276,400)) and reddish(px(640,796)) and px(40,400)[0] > 25'
assert_shot "glow fades toward the middle" glow 1280 'black(px(640,400)) and px(3,400)[0] > px(60,400)[0] > px(200,400)[0]'
assert_shot "no arrow in the glow look" glow 1280 'not clay(px(312,212))'

# the glow pulses: the edge brightness changes over a second
shot pulse1 HEADLESS-1; sleep 1.0; shot pulse2 HEADLESS-1
if python3 -I -c "
from PIL import Image
a=Image.open('$EVID/pulse1.png').getpixel((3,400))[0]; b=Image.open('$EVID/pulse2.png').getpixel((3,400))[0]
import sys; print('    edge red', a, b); sys.exit(0 if abs(a-b) >= 8 else 1)"; then ok "glow pulses (alpha modifier)"; else bad "glow did not pulse"; fi

# second output at scale 1.5 with an x offset
send '{"cmd":"look","look":"rim"}'
send '{"cmd":"move","output":"HEADLESS-2"}'
wait_event '"event":"shown","output":"HEADLESS-2","scale":1\.5' && ok "move: shown on HEADLESS-2 at scale 1.5" || bad "move shown"
send '{"cmd":"jump","x":1680,"y":300}'
sleep 0.5
shot out2 HEADLESS-2
shot out1_after_move HEADLESS-1
assert_shot "HEADLESS-1 empty after the move" out1_after_move 1280 'all_black()'
assert_shot "rim on HEADLESS-2 (scale 1.5)" out2 1280 'clay(px(0.3,400)) and clay(px(1279.5,400)) and clay(px(640,0.3)) and clay(px(640,799.5))'
assert_shot "arrow at global (1680,300) = local (400,300) on HEADLESS-2" out2 1280 'clay(px(412,312)) and region(396,295,404,303,white) > 4 and black(px(390,290))'
python3 -I -c "
from PIL import Image; im=Image.open('$EVID/out2.png'); print('    HEADLESS-2 screenshot', im.size)"

# press: ring around the press point, then gone
send '{"cmd":"press","x":1880,"y":500}'
sleep 0.18
shot press HEADLESS-2
assert_shot "press ring around (600,500) on HEADLESS-2, arrow snapped there" press 1280 'region(560,460,598,498,reddish) > 20 and clay(px(612,512))'
sleep 1.3
shot press_after HEADLESS-2
assert_shot "ring gone after 1.1 s" press_after 1280 'region(560,460,598,498,reddish) == 0 and clay(px(612,512))'

# glide: plays the frames, rests on the last one
FR="$(python3 -I -c "
import json, math
f=[[1880+i*(300/29), 500-i*(200/29), 3*math.sin(i/5), min(1, i/10)] for i in range(30)]+[[2180,300,0,0]]
print(json.dumps(f))")"
send "{\"cmd\":\"glide\",\"frames\":$FR,\"frameMs\":16}"
sleep 0.15
shot glide_mid HEADLESS-2
sleep 0.6
shot glide_end HEADLESS-2
assert_shot "glide mid-flight: arrow between start and end" glide_mid 1280 'region(620,220,900,520,clay) > 30 and not clay(px(612,512))'
assert_shot "glide rests on the last frame (900,300 local)" glide_end 1280 'clay(px(912,312)) and region(600,480,640,530,clay) == 0'

# capture hide/restore
send '{"cmd":"capture","phase":"hide","id":5}'
wait_event '"event":"ack","id":5' && ok "capture hide acked" || bad "capture hide ack"
shot cap_hidden HEADLESS-2
assert_shot "fully transparent during a capture hide" cap_hidden 1280 'all_black()'
send '{"cmd":"capture","phase":"restore","id":6}'
wait_event '"event":"ack","id":6' && ok "capture restore acked" || bad "capture restore ack"
shot cap_restored HEADLESS-2
assert_shot "back after restore" cap_restored 1280 'clay(px(0.3,400)) and clay(px(912,312))'

# capture hide without restore: auto-restore after 3 s
send '{"cmd":"capture","phase":"hide","id":7}'
wait_event '"event":"ack","id":7' || bad "ack 7"
wait_event '"event":"error","message":"capture: no restore.*"fatal":false' 4 && ok "auto-restore reported: $LAST" || bad "auto-restore event"
sleep 0.2
shot cap_auto HEADLESS-2
assert_shot "overlay back after the auto-restore" cap_auto 1280 'clay(px(0.3,400)) and clay(px(912,312))'

# malformed input keeps the overlay running
send 'this is not json'
wait_event '"event":"error".*"fatal":false' && ok "malformed line: non-fatal error" || bad "malformed"
send '{"cmd":"warp"}'
wait_event '"event":"error".*"fatal":false' && ok "unknown command: non-fatal error" || bad "unknown cmd"
send '{"cmd":"move","output":"NOPE-9"}'
wait_event '"event":"error","message":"move: unknown output: NOPE-9","fatal":false' && ok "move to an unknown output: non-fatal error" || bad "move unknown"
sleep 0.3
kill -0 "$OPID" 2>/dev/null && ok "still running after the bad move" || bad "died on a bad move"
shot move_bad HEADLESS-2
assert_shot "still shown on HEADLESS-2 after the bad move" move_bad 1280 'clay(px(0.3,400)) and clay(px(912,312))'

# hide: fade, destroy, nothing left
send '{"cmd":"hide"}'
wait_event '"event":"hidden"' && ok "hidden" || bad "hidden"
shot hidden2 HEADLESS-2; shot hidden1 HEADLESS-1
assert_shot "nothing left on HEADLESS-2 after hide" hidden2 1280 'all_black()'
assert_shot "nothing left on HEADLESS-1 after hide" hidden1 1280 'all_black()'

# a later show creates a new surface
send '{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":null}'
wait_event '"event":"shown","output":"HEADLESS-1"' && ok "show after hide" || bad "re-show"
sleep 0.4
shot reshow HEADLESS-1
assert_shot "re-shown rim, no arrow (arrow:null)" reshow 1280 'clay(px(0,400)) and region(200,100,1000,700,clay) == 0'

# click-through: a click at the arrow reaches the window below
if command -v foot >/dev/null; then
  printf '[colors]\nbackground=203040\n' >"$RUN/foot.ini"
  swaymsg -q "exec foot -c $RUN/foot.ini -T left-win sleep 600"
  sleep 1.0
  swaymsg -q "exec foot -c $RUN/foot.ini -T right-win sleep 600"
  sleep 1.2
  focused() { swaymsg -t get_tree | python3 -I -c "
import json,sys
def walk(n):
    if n.get('focused') and n.get('name'): print(n['name']); return True
    return any(walk(c) for c in n.get('nodes',[])+n.get('floating_nodes',[]))
walk(json.load(sys.stdin))"; }
  [ "$(focused)" = "right-win" ] && ok "right window focused before the click" || bad "focus setup: $(focused)"
  send '{"cmd":"jump","x":320,"y":400}'
  sleep 0.3
  shot click_before HEADLESS-1
  assert_shot "arrow drawn over the left window" click_before 1280 'clay(px(332,412))'
  swaymsg -q "seat - cursor set 320 400"; sleep 0.2
  swaymsg -q "seat - cursor press button1"; sleep 0.1; swaymsg -q "seat - cursor release button1"; sleep 0.3
  [ "$(focused)" = "left-win" ] && ok "click at the arrow focused the window below (overlay is click-through)" \
    || bad "click did not reach the window below: focus=$(focused)"
  swaymsg -q '[title="left-win"] kill'; swaymsg -q '[title="right-win"] kill'
  sleep 0.3
else
  echo "  SKIP click-through (foot not installed)"
fi

if [ "$CPU" = 1 ]; then
  echo ">>> CPU of the overlay process (10 s each)"
  ticks() { awk '{print $14+$15}' "/proc/$OPID/stat"; }
  hz=$(getconf CLK_TCK)
  measure() { # measure <label> <driver fn>
    local t0 t1; t0=$(ticks); local s=$SECONDS
    "$2"
    t1=$(ticks)
    python3 -I -c "print('    %-34s %5.2f %% of one core' % ('$1', 100.0 * ($t1 - $t0) / $hz / 10))"
  }
  idle() { sleep 10; }
  driver() {
    local i
    for i in $(seq 1 10); do
      send "{\"cmd\":\"glide\",\"frames\":$FR,\"frameMs\":16}"; sleep 0.6
      send '{"cmd":"press","x":2180,"y":300}'; sleep 0.4
    done
  }
  send '{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":{"x":300,"y":300}}'; sleep 1
  measure "rim, idle" idle
  send '{"cmd":"look","look":"glow"}'; sleep 1
  measure "glow, pulsing (alpha modifier)" idle
  send '{"cmd":"move","output":"HEADLESS-2"}'; send '{"cmd":"look","look":"rim"}'; sleep 1
  measure "rim, glide + press every second" driver
fi

# EOF ends the overlay with 0
exec 3>&-
if wait "$OPID"; then ok "stdin EOF: exit 0"; else bad "exit code $? on EOF"; fi
[ -s "$RUN/a.err" ] && { echo "    overlay stderr:"; cat "$RUN/a.err"; }

echo ">>> overlay B (no alpha modifier: static glow, instant show/hide, buffer-based capture hide)"
SEEN=0
start_overlay b CU_OVERLAY_DISABLE=alpha-modifier
wait_event '"event":"ready".*"alphaModifier":false' && ok "ready without alpha modifier" || bad "ready b"
send '{"cmd":"show","output":"HEADLESS-1","look":"glow"}'
wait_event '"event":"shown"' || bad "shown b"
sleep 0.3
shot b_glow HEADLESS-1; sleep 0.8; shot b_glow2 HEADLESS-1
assert_shot "static glow drawn" b_glow 1280 'reddish(px(3,400)) and black(px(640,400))'
if python3 -I -c "
from PIL import Image
a=Image.open('$EVID/b_glow.png').getpixel((3,400)); b=Image.open('$EVID/b_glow2.png').getpixel((3,400))
import sys; sys.exit(0 if max(abs(x-y) for x,y in zip(a,b)) <= 4 else 1)"; then ok "glow static without alpha modifier"; else bad "glow changed"; fi
send '{"cmd":"look","look":"rim"}'
send '{"cmd":"jump","x":500,"y":300}'
send '{"cmd":"capture","phase":"hide","id":1}'
wait_event '"event":"ack","id":1' || bad "ack b1"
shot b_cap HEADLESS-1
assert_shot "capture hide without alpha modifier is fully transparent" b_cap 1280 'all_black()'
send '{"cmd":"capture","phase":"restore","id":2}'
wait_event '"event":"ack","id":2' || bad "ack b2"
shot b_restored HEADLESS-1
assert_shot "restored without alpha modifier" b_restored 1280 'clay(px(0,400)) and clay(px(512,312))'
send '{"cmd":"hide"}'
wait_event '"event":"hidden"' 1 && ok "instant hide" || bad "hide b"
shot b_hidden HEADLESS-1
assert_shot "nothing left (no alpha modifier)" b_hidden 1280 'all_black()'
exec 3>&-
wait "$OPID" || bad "overlay b exit $?"

echo ">>> overlay E (no viewporter: integer buffer scale on the 1.5 output)"
SEEN=0
start_overlay e CU_OVERLAY_DISABLE=viewporter
wait_event '"event":"ready".*"fractionalScale":false' && ok "ready without viewporter / fractional scale" || bad "ready e"
send '{"cmd":"show","output":"HEADLESS-2","look":"rim","arrow":{"x":1680,"y":300}}'
wait_event '"event":"shown","output":"HEADLESS-2","scale":2\.0' && ok "shown at integer scale 2 (buffer scale)" || bad "shown e"
sleep 0.4
shot e_rim HEADLESS-2
assert_shot "rim and arrow without viewporter" e_rim 1280 'clay(px(0.3,400)) and clay(px(1279.5,400)) and clay(px(412,312)) and region(396,295,404,303,white) > 4'
exec 3>&-
wait "$OPID" || bad "overlay e exit $?"

echo ">>> overlay F (output hot-plug and unplug while shown)"
SEEN=0
start_overlay f
wait_event '"event":"ready"' || bad "ready f"
swaymsg -q create_output
swaymsg -q output HEADLESS-3 resolution 800x600 position 2560 0 scale 1
wait_event '"event":"outputs".*"name":"HEADLESS-3","x":2560,"y":0,"width":800,"height":600,"scale":1\.0' \
  && ok "outputs event on hot-plug: $LAST" || bad "no outputs event for HEADLESS-3"
send '{"cmd":"show","output":"HEADLESS-3","look":"rim","arrow":{"x":2700,"y":100}}'
wait_event '"event":"shown","output":"HEADLESS-3"' && ok "shown on the hot-plugged HEADLESS-3" || bad "shown f"
sleep 0.3
shot f_on3 HEADLESS-3
assert_shot "rim and arrow on HEADLESS-3 (global 2700,100 = local 140,100)" f_on3 800 'clay(px(0,300)) and clay(px(152,112))'
if swaymsg -q output HEADLESS-3 unplug; then
  wait_event '"event":"error","message":"the compositor closed the overlay surface","fatal":false' && ok "unplug: non-fatal error" || bad "unplug error event"
  wait_event '"event":"hidden"' && ok "unplug: hidden" || bad "unplug hidden"
  sleep 0.3
  grep -q '"event":"outputs","outputs":\[{"name":"HEADLESS-1".*"HEADLESS-2"[^]]*\]}$' "$OUT" && ! tail -n1 "$OUT" | grep -q HEADLESS-3 \
    && ok "outputs event without HEADLESS-3" || { tail -n3 "$OUT"; bad "outputs event after unplug"; }
  kill -0 "$OPID" 2>/dev/null && ok "overlay alive after the unplug" || bad "overlay died on unplug"
  send '{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":{"x":300,"y":200}}'
  wait_event '"event":"shown","output":"HEADLESS-1"' && ok "show on a remaining output after the unplug" || bad "re-show f"
  sleep 0.3
  shot f_on1 HEADLESS-1
  assert_shot "rim and arrow on HEADLESS-1 after the unplug" f_on1 1280 'clay(px(0,400)) and clay(px(312,212))'
else
  echo "  SKIP unplug (this sway cannot unplug a headless output)"
fi
exec 3>&-
wait "$OPID" || bad "overlay f exit $?"

echo ">>> overlay C (unknown output is fatal)"
SEEN=0
start_overlay c
wait_event '"event":"ready"' || bad "ready c"
send '{"cmd":"show","output":"NOPE-9","look":"rim"}'
wait_event '"event":"error","message":"unknown output: NOPE-9","fatal":true' && ok "unknown output: fatal error" || bad "fatal event"
rc=0; wait "$OPID" || rc=$?
[ "$rc" = 2 ] && ok "unknown output: exit 2" || bad "unknown output exit $rc"
exec 3>&-

echo ">>> overlay D (parent death)"
SEEN=0
( sleep 600 ) & HOLDER=$!
PIDS+=("$HOLDER")
start_overlay d "CLAUDE_CU_PARENT_PID=$HOLDER"
wait_event '"event":"ready"' || bad "ready d"
send '{"cmd":"show","output":"HEADLESS-1","look":"rim"}'
wait_event '"event":"shown"' || bad "shown d"
kill "$HOLDER"
t0=$SECONDS; rc=0
for _ in $(seq 1 30); do kill -0 "$OPID" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$OPID" 2>/dev/null; then bad "overlay outlived CLAUDE_CU_PARENT_PID"; else ok "exits when CLAUDE_CU_PARENT_PID dies"; fi
wait "$OPID" 2>/dev/null || true
exec 3>&-
sleep 0.2
shot d_after HEADLESS-1
assert_shot "no surface left after parent death" d_after 1280 'all_black()'

echo
echo "passed: $pass, failed: $fail"
[ -n "${EVIDENCE_DIR:-}" ] && echo "evidence: $EVID"
[ "$fail" -eq 0 ]
