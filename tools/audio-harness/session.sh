#!/usr/bin/env bash

set -euo pipefail

USAGE="usage: session.sh --server pipewire|pulseaudio -- COMMAND [ARGUMENT...]"
SINKS=("harness_a 48000 2" "harness_b 48000 2" "harness_hfp 16000 1")
DEFAULT_SINK=harness_a
STARTUP_POLLS=100

fail() { echo "session.sh: $*" >&2; exit 1; }

if [ "$#" -lt 4 ] || [ "$1" != --server ] || [ "$3" != -- ]; then
    fail "$USAGE"
fi
server="$2"
shift 3
case "$server" in
pipewire) daemons=(pipewire wireplumber pipewire-pulse) ;;
pulseaudio) daemons=(pulseaudio) ;;
*) fail "$USAGE" ;;
esac
for tool in dbus-run-session pactl "${daemons[@]}"; do
    command -v "$tool" >/dev/null || fail "$tool not found; install it and re-run"
done

if [ -z "${ECLIPSE_AUDIO_HARNESS_BUS:-}" ]; then
    ECLIPSE_AUDIO_HARNESS_BUS=1 exec dbus-run-session -- "$0" --server "$server" -- "$@"
fi
unset ECLIPSE_AUDIO_HARNESS_BUS

root="$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/eclipse-audio.XXXXXX")"
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true; wait; rm -rf "$root"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -m 700 "$root/runtime"
mkdir -p "$root/home" "$root/state" "$root/config/pulse" "$root/config/alsa"
unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PIPEWIRE_CONFIG_DIR PIPEWIRE_NODE PULSE_SERVER \
    PULSE_SINK PULSE_SOURCE PULSE_COOKIE PULSE_CLIENTCONFIG DISPLAY WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$root/runtime" XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state"
export GIO_USE_VFS=local
echo "autospawn = no" >"$XDG_CONFIG_HOME/pulse/client.conf"

start() {
    HOME="$root/home" "$@" >"$root/$1.log" 2>&1 &
    pids+=("$!")
}

disable_wireplumber_04_monitors() {
    local config="$XDG_CONFIG_HOME/wireplumber"
    mkdir -p "$config/main.lua.d" "$config/bluetooth.lua.d"
    printf '%s.enabled = false\n' alsa_monitor v4l2_monitor libcamera_monitor \
        >"$config/main.lua.d/60-eclipse-audio-harness.lua"
    echo "bluez_monitor.enabled = false" >"$config/bluetooth.lua.d/60-eclipse-audio-harness.lua"
}

case "$server" in
pipewire)
    start pipewire
    if wireplumber --version | grep -q 'libwireplumber 0\.4\.'; then
        disable_wireplumber_04_monitors
        start wireplumber
    else
        start wireplumber --profile policy
    fi
    start pipewire-pulse
    ;;
pulseaudio)
    echo "load-module module-native-protocol-unix auth-anonymous=1" >"$root/server.pa"
    start pulseaudio -n --daemonize=no --use-pid-file=no --exit-idle-time=-1 \
        --file="$root/server.pa"
    ;;
esac

socket="$XDG_RUNTIME_DIR/pulse/native"
export PULSE_SERVER="unix:$socket"
polls=0
until pactl info >/dev/null 2>&1; do
    polls=$((polls + 1))
    if [ "$polls" -ge "$STARTUP_POLLS" ]; then
        tail -n 20 "$root"/*.log >&2
        fail "the private $server server did not answer at $socket"
    fi
    sleep 0.1
done

answered="$(LC_ALL=C pactl info | sed -n 's/^Server String: //p')"
if [ "$answered" != "$PULSE_SERVER" ]; then
    fail "refusing to run: pactl reached $answered, not $PULSE_SERVER"
fi

for sink in "${SINKS[@]}"; do
    read -r name rate channels <<<"$sink"
    pactl load-module module-null-sink "sink_name=$name" "rate=$rate" "channels=$channels" \
        "sink_properties=device.description=$name" >/dev/null
done
expected="$(for sink in "${SINKS[@]}"; do echo "${sink%% *}"; done | sort)"
polls=0
until listed="$(LC_ALL=C pactl list short sinks | cut -f2 | sort)" && [ "$listed" = "$expected" ]
do
    unexpected="$(comm -23 <(echo "$listed") <(echo "$expected") | tr '\n' ' ')"
    if [ -n "${unexpected// /}" ]; then
        fail "refusing to run: the private server has other sinks: $unexpected"
    fi
    polls=$((polls + 1))
    if [ "$polls" -ge "$STARTUP_POLLS" ]; then
        fail "the harness sinks did not appear; the server lists: $listed"
    fi
    sleep 0.1
done
pactl set-default-sink "$DEFAULT_SINK"

printf 'pcm.!default { type pulse }\nctl.!default { type pulse }\n' \
    >"$XDG_CONFIG_HOME/alsa/asoundrc"
export ECLIPSE_AUDIO_HARNESS="$root"

status=0
"$@" || status=$?
exit "$status"
