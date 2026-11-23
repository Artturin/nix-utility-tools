# Take the README screenshots of nix-utility-gui: the Run and Results views,
# in light and dark, written to docs/screenshots/ (or the directory given).
# Run with `nix run .#screenshots` from the repository root; flake.nix provides
# the programs and FIXTURE, SETTINGS, ICONS and FONTCONFIG_FILE.
#
# Everything runs on a private X server and D-Bus session with throwaway
# settings, the host's settings and portals left out, the fonts pinned and the
# cairo renderer, so that the same inputs give the same pixels anywhere.

if [ -z "${SCREENSHOTS_IN_SESSION:-}" ]; then
    if [ $# -eq 0 ] && [ ! -f flake.nix ]; then
        echo "run this from the repository root, or give an output directory" >&2
        exit 1
    fi
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    # A bus without service directories: nothing from the host (portals,
    # dconf) can be started on it.
    cat >"$work/bus.conf" <<END
<busconfig>
  <type>session</type>
  <listen>unix:dir=$work</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
END
    SCREENSHOTS_IN_SESSION=1 SCREENSHOTS_WORK=$work \
        dbus-run-session --config-file="$work/bus.conf" -- "$0" "$@"
    exit
fi

work=$SCREENSHOTS_WORK
out=$(realpath "${1:-docs/screenshots}")
mkdir -p "$out"

cp -r --no-preserve=mode "$FIXTURE" "$work/fixture"
results=$(find "$work/fixture" -mindepth 1 -maxdepth 1 -name 'differences-*' -print -quit)
mkdir -p "$work/config/nix-utility-gui"
cp --no-preserve=mode "$SETTINGS" "$work/config/nix-utility-gui/settings.toml"

export XDG_CONFIG_HOME=$work/config XDG_DATA_HOME=$work/data XDG_CACHE_HOME=$work/cache
export GDK_BACKEND=x11 GSK_RENDERER=cairo GDK_DEBUG=no-portals GTK_A11Y=none
export GSETTINGS_BACKEND=memory LANG=C.UTF-8 TZ=UTC XDG_DATA_DIRS=$ICONS
export ADW_DISABLE_PORTAL=1 ADW_DEBUG_ACCENT_COLOR=blue ADW_DEBUG_HIGH_CONTRAST=0

Xvfb -displayfd 3 -screen 0 1280x900x24 -nolisten tcp 3>"$work/display" 2>/dev/null &
xvfb=$!
until [ -s "$work/display" ]; do sleep 0.1; done
DISPLAY=:$(cat "$work/display")
export DISPLAY

# shot NAME [ARGS...]: launch the app, capture its window, close it.
shot() {
    local name=$1
    shift
    nix-utility-gui "$@" </dev/null >"$work/$name.log" 2>&1 &
    local app=$!
    local window
    window=$(xdotool search --sync --onlyvisible --name '^Nix utility tools$' | head -n1)
    sleep 5
    import -window "$window" png:- |
        magick png:- -strip -define png:exclude-chunks=date,time "$out/$name.png"
    kill "$app"
    wait "$app" || true
    echo "$out/$name.png"
}

for scheme in light dark; do
    export ADW_DEBUG_COLOR_SCHEME=prefer-$scheme
    shot "run-$scheme"
    shot "results-$scheme" "$results"
done

kill "$xvfb"
wait "$xvfb" || true
