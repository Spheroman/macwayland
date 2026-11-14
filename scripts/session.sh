#!/bin/sh
# Start the full-screen session with a Linux terminal (foot) in it:
#   scripts/session.sh                 foot + bash from nixpkgs (aarch64-linux)
#   MACWL_ROOT=<dir> scripts/session.sh  foot inside an extracted distro (its /bin/sh)
# Cmd+Q quits. Needs: cargo build --release, nix (for foot), signed binaries.
#
# The session's Linux processes share one host (one Mac process, one VM:
# docs/processes.md): the script starts it (`macwl-run --serve`) and exports MACWL_HOST, so the
# D-Bus daemon, foot and everything started from the session (a Mac
# terminal that sources its environment too) join it as its clients.
# `macwl-run --ps` lists its processes.
# Linux programs there can run Mac ones (open, pbcopy, git, ...), each a Mac
# process of its own that the Linux process stands in for.
#
# For tests: MACWL_SESSION_COMPOSITOR=headless runs the headless compositor
# (macwl-compositor, typed at with MACWL_TYPE, its frames dumped in the
# session's directory) in place of the shell; MACWL_SESSION_DIR is the
# session's directory (default /tmp/macwl-session-$UID, emptied first).
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
if [ "$MACWL_SESSION_COMPOSITOR" = headless ]; then
    compositor=macwl-compositor
else
    compositor=macwl-shell
fi
cargo build -q --release -p $compositor -p macwl-runtime
scripts/sign-hv.sh target/release/macwl-run >/dev/null 2>&1

linux_pkg() { nix build "nixpkgs#legacyPackages.aarch64-linux.$1.out" --max-jobs 0 --no-link --print-out-paths; }
foot=$(linux_pkg foot)
dbus=$(linux_pkg dbus)

runtime=${MACWL_SESSION_DIR:-/tmp/macwl-session-$(id -u)}
rm -rf "$runtime"
mkdir -p "$runtime"
chmod 700 "$runtime"
if [ $compositor = macwl-compositor ]; then
    # Its frames go where it runs.
    (cd "$runtime" && XDG_RUNTIME_DIR=$runtime exec "$root/target/release/$compositor") &
else
    XDG_RUNTIME_DIR=$runtime target/release/$compositor &
fi
shell=$!
trap 'kill $shell 2>/dev/null' EXIT
while [ ! -S "$runtime/wayland-mac" ]; do
    kill -0 $shell 2>/dev/null || exit 1
    sleep 0.1
done

export WAYLAND_DISPLAY=wayland-mac XDG_RUNTIME_DIR=$runtime

# One host for the session: everything below joins it.
export MACWL_HOST="$runtime/macwl-host"
# Mac programs are the host's to start (docs/processes.md, section 7).
MACWL_MAC_PROGRAMS=1 target/release/macwl-run --serve "$MACWL_HOST" </dev/null &
host=$!
trap 'kill $shell $host 2>/dev/null' EXIT
while [ ! -S "$MACWL_HOST" ]; do
    kill -0 $host 2>/dev/null || exit 1
    sleep 0.05
done

# A D-Bus session bus (nixpkgs' dbus-daemon, under macwl-run too).
target/release/macwl-run "$dbus/bin/dbus-daemon" --config-file="$dbus/share/dbus-1/session.conf" \
    --address="unix:path=$runtime/bus" --nofork &
bus=$!
# (The host goes by itself 2 s after its last process; told to end, it
# ends the session's processes.)
trap 'kill $shell $bus $host 2>/dev/null' EXIT
while [ ! -S "$runtime/bus" ]; do
    kill -0 $bus 2>/dev/null || break
    sleep 0.1
done
export DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus"

if [ -n "$MACWL_ROOT" ]; then
    target/release/macwl-run --root "$MACWL_ROOT" "$foot/bin/foot"
else
    # The Mac's /bin holds Mac programs: give foot a Linux shell, and the
    # shell Linux tools first in its PATH, then the Mac's (run as Mac
    # programs: open, pbcopy, git, ...).
    bash=$(linux_pkg bashInteractive)
    path="$bash/bin"
    for pkg in coreutils fastfetch findutils gnugrep gnused less ncurses procps vim; do
        path="$path:$(linux_pkg $pkg)/bin"
    done
    path="$path:$PATH"
    # Nix's glibc has only C.UTF-8 built in: without locale data, the Mac's
    # LANG (en_US.UTF-8, say) would leave everything in ASCII ("?" for the rest).
    locales=$(nix build "nixpkgs#legacyPackages.aarch64-linux.glibcLocalesUtf8" --max-jobs 0 --no-link --print-out-paths)
    # What GTK apps (Nix's, started from the shell) expect NixOS to have set:
    # icon themes, the SVG loader for their icons, GStreamer's plugins.
    data="$(linux_pkg adwaita-icon-theme)/share:$(linux_pkg hicolor-icon-theme)/share"
    pixbuf="$(linux_pkg librsvg)/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
    gst="$(linux_pkg gst_all_1.gst-plugins-base)/lib/gstreamer-1.0"
    # NOSYSBASHRC: the Mac's /etc/bashrc would put the Mac's PATH back.
    MACWL_ENV_PATH=$path MACWL_ENV_NOSYSBASHRC=1 MACWL_ENV_LOCALE_ARCHIVE=$locales/lib/locale/locale-archive \
    MACWL_ENV_XDG_DATA_DIRS=$data MACWL_ENV_GDK_PIXBUF_MODULE_FILE=$pixbuf MACWL_ENV_GST_PLUGIN_SYSTEM_PATH_1_0=$gst \
        target/release/macwl-run "$foot/bin/foot" "$bash/bin/bash"
fi
