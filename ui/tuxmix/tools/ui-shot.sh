#!/usr/bin/env bash
# Take a screenshot of the simulated mixer with chosen panels open.
#
#   tools/ui-shot.sh /tmp/in12.png --flyouts settings:input:0
#   tools/ui-shot.sh /tmp/all.png \
#       --flyouts settings:input:0,settings:input:2,settings:playback:0,settings:output:2
#   tools/ui-shot.sh /tmp/outs.png --scale 0.75 \
#       --flyouts settings:output:0,settings:output:2
#
# Everything after the output path is passed to tuxmix-gui, so the normal
# automation flags work here: --view mixer|matrix|quick, --scale 0.75..1.75,
# --select-out N and --flyouts kind:channel:index[,kind:channel:index...].
# Use --scale 0.75 when the hardware-output row must be visible: the full
# three-row mixer plus a panel is taller than the window at 100 %.
#
# The instance is a mock with a throw-away XDG_CONFIG_HOME: it never touches
# the user's preferences, scene or hardware.  The window is placed through
# KWin when available and captured with Spectacle.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-/tmp/tuxmix-shot.png}"
shift || true

cfg="$(mktemp -d "${TMPDIR:-/tmp}/tuxmix-shot.XXXXXX")"
bin="$root/target/debug/tuxmix-gui"

cleanup() {
    if [[ -n "${pid:-}" ]] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
    fi
    rm -rf "$cfg"
}
trap cleanup EXIT

(cd "$root" && cargo build -q -p tuxmix-gui)

# A readable default; the caller can override with --scale.
if [[ " $* " != *" --scale "* ]]; then
    set -- --scale 1.25 "$@"
fi

XDG_CONFIG_HOME="$cfg" DISPLAY="${DISPLAY:-:0}" \
    "$bin" --mock-ucx --view mixer "$@" >"$cfg/app.log" 2>&1 &
pid=$!

# Wait for the window and for the first frame.
for _ in $(seq 1 60); do
    kill -0 "$pid" 2>/dev/null || { echo "tuxmix-gui exited early:"; cat "$cfg/app.log"; exit 1; }
    sleep 0.1
done
sleep 2

# Place and activate the simulated window; Spectacle then captures it.
if command -v qdbus >/dev/null 2>&1; then
    cat >"$cfg/place.js" <<'JS'
const wins = workspace.windowList();
for (const w of wins) {
    const cap = (w.caption || "").toString();
    if (cap.indexOf("simulated") !== -1) {
        w.frameGeometry = { x: 0, y: 0, width: 1600, height: 950 };
        workspace.activeWindow = w;
    }
}
JS
    qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript "$cfg/place.js" >/dev/null 2>&1 || true
    qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.start >/dev/null 2>&1 || true
    sleep 1
fi

if command -v spectacle >/dev/null 2>&1; then
    DISPLAY="${DISPLAY:-:0}" spectacle -b -n -a -o "$out" >/dev/null 2>&1 ||
        DISPLAY="${DISPLAY:-:0}" spectacle -b -n -f -o "$out" >/dev/null 2>&1
elif command -v import >/dev/null 2>&1; then
    DISPLAY="${DISPLAY:-:0}" import -window root "$out"
else
    echo "Need spectacle or ImageMagick import to take the screenshot." >&2
    exit 1
fi

echo "$out"
