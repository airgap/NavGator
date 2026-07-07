#!/usr/bin/env python3
"""Bundle GStreamer + every non-system shared library the engine needs into a Linux
AppDir, so the resulting AppImage runs on a machine WITHOUT any gstreamer1.0-plugins-*
packages installed.

This is the Linux sibling of scripts/macos-bundle-gst.py. NavGator builds with
`cargo build -p navgator` (media-gstreamer on), so the binary dlopen's GStreamer at
runtime; a bare AppImage that only ships the binary crashes/degrades on any host missing
the GStreamer plugins. libservo loads a curated plugin set (components/servo/
gstreamer_plugin_lists) which are dlopen'd, so `ldd` alone never sees them — we copy the
curated plugins + their transitive libs explicitly.

Strategy (LD_LIBRARY_PATH based — no rpath patching):
  * ldd the binary + each curated plugin, recursively, and copy every resolved lib that
    is NOT in EXCLUDE (the "must come from the host" set: glibc, the GL/GPU driver stack,
    X11/xcb/wayland/drm) into <appdir>/usr/lib.
  * copy the curated plugins into <appdir>/usr/lib/gstreamer-1.0 and gst-plugin-scanner
    next to them.
AppRun then sets LD_LIBRARY_PATH + GST_PLUGIN_SYSTEM_PATH + GST_PLUGIN_SCANNER so the
loader and GStreamer find the bundled copies.
"""
import argparse
import ast
import os
import re
import shutil
import subprocess
import sys

# Fallback if the swervo plugin lists can't be located (keep in sync with
# components/servo/gstreamer_plugin_lists/common.rs.in — the Linux build uses common only).
FALLBACK_PLUGINS = [
    "gstcoreelements", "gstnice", "gstapp", "gstaudioconvert", "gstaudioresample",
    "gstgio", "gstogg", "gstopengl", "gstopus", "gstplayback", "gsttheora",
    "gsttypefindfunctions", "gstvideoconvertscale", "gstvolume", "gstvorbis",
    "gstaudiofx", "gstaudioparsers", "gstautodetect", "gstdeinterlace", "gstid3demux",
    "gstinterleave", "gstisomp4", "gstmatroska", "gstrtp", "gstrtpmanager",
    "gstvideofilter", "gstvpx", "gstwavparse", "gstaudiobuffersplit", "gstdtls",
    "gstid3tag", "gstproxy", "gstvideoparsersbad", "gstwebrtc", "gstlibav",
]

# Libraries that MUST come from the host, not the bundle: glibc (matches the kernel), the
# GL/GPU driver stack + libdrm/gbm/vulkan (must match the installed GPU driver), and the
# X11/xcb/wayland client libs (must match the running display server). Bundling any of
# these is the classic "AppImage works here, black screen / GLXBadContext there" failure.
# Matched by basename prefix (before the first ".so"). Mirrors the pkg2appimage excludelist,
# trimmed to the families that actually matter for a GPU-compositing browser.
EXCLUDE_PREFIXES = (
    # glibc / loader
    "ld-linux", "libc", "libm", "libdl", "libpthread", "librt", "libresolv",
    "libutil", "libnsl", "libnss_", "libBrokenLocale", "libanl", "libmvec",
    "libthread_db", "libpcprofile",
    # GL / GPU driver stack
    "libGL", "libEGL", "libGLX", "libGLdispatch", "libOpenGL", "libGLU", "libGLESv",
    "libglapi", "libgbm", "libdrm", "libvulkan",
    # X11 / display server — match the host
    "libX11", "libxcb", "libXext", "libXrender", "libXi", "libXrandr", "libXfixes",
    "libXcursor", "libXinerama", "libXdamage", "libXcomposite", "libXtst", "libXss",
    "libXau", "libXdmcp", "libxshmfence",
    "libwayland-",
)


def excluded(basename):
    stem = basename.split(".so", 1)[0]
    return any(stem == p or stem.startswith(p) for p in EXCLUDE_PREFIXES)


_LDD_RE = re.compile(r"=>\s*(/[^\s]+)")


def ldd_deps(path):
    """Resolved (real, absolute) shared-lib paths of `path`, via ldd. Only lines with a
    `=> /abs/path` mapping; the loader/vdso lines have no path and are skipped."""
    out = set()
    res = subprocess.run(["ldd", path], capture_output=True, text=True)
    for line in res.stdout.splitlines():
        m = _LDD_RE.search(line)
        if m:
            out.add(m.group(1))
    return out


def load_plugins(plugin_lists_dir):
    names = []
    fp = os.path.join(plugin_lists_dir or "", "common.rs.in")
    if plugin_lists_dir and os.path.exists(fp):
        stripped = [ln for ln in open(fp) if not ln.strip().startswith("//")]
        try:
            names = ast.literal_eval(" ".join(stripped))
        except (SyntaxError, ValueError):
            names = []
    if not names:
        print("linux gst bundle: using built-in fallback plugin list", file=sys.stderr)
        names = FALLBACK_PLUGINS
    return [f"lib{n}.so" for n in names]


def copy_lib(src, lib_dir):
    """Copy a resolved lib into lib_dir under its real basename (deref symlinks). Returns
    the dest path if it was newly copied, else None."""
    real = os.path.realpath(src)
    dest = os.path.join(lib_dir, os.path.basename(real))
    if os.path.exists(dest):
        return None
    shutil.copyfile(real, dest)
    os.chmod(dest, 0o644)
    # Also drop a symlink under the SONAME the loader asks for, if it differs from the
    # real basename (e.g. libfoo.so.1 -> libfoo.so.1.2.3), so LD_LIBRARY_PATH lookups hit.
    asked = os.path.basename(src)
    if asked != os.path.basename(real):
        link = os.path.join(lib_dir, asked)
        if not os.path.exists(link):
            os.symlink(os.path.basename(real), link)
    return dest


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True, help="the ELF executable in the AppDir")
    ap.add_argument("--lib-dir", required=True, help="output libs dir, e.g. AppDir/usr/lib")
    ap.add_argument("--gst-plugin-dir", required=True,
                    help="host gstreamer plugin dir, e.g. /usr/lib/x86_64-linux-gnu/gstreamer-1.0")
    ap.add_argument("--plugin-lists", default="", help="swervo gstreamer_plugin_lists dir (optional)")
    ap.add_argument("--scanner", default="", help="path to gst-plugin-scanner (optional)")
    args = ap.parse_args()

    lib_dir = args.lib_dir
    plugin_out = os.path.join(lib_dir, "gstreamer-1.0")
    os.makedirs(plugin_out, exist_ok=True)

    # Seed the work list: the binary's own deps + the curated plugins.
    pending = set(ldd_deps(args.binary))
    missing = []
    for plugin in load_plugins(args.plugin_lists):
        pp = os.path.join(args.gst_plugin_dir, plugin)
        if os.path.exists(pp):
            dest = os.path.join(plugin_out, plugin)
            if not os.path.exists(dest):
                shutil.copyfile(pp, dest)
                os.chmod(dest, 0o644)
            pending.update(ldd_deps(pp))   # the plugin's own lib deps (e.g. gstlibav -> libav*)
        else:
            missing.append(plugin)
    if missing:
        print("linux gst bundle: WARN plugins not found: " + ", ".join(missing), file=sys.stderr)

    # gst-plugin-scanner runs in a subprocess to introspect plugins; bundle it + its deps.
    if args.scanner and os.path.exists(args.scanner):
        scan_dest = os.path.join(plugin_out, "gst-plugin-scanner")
        shutil.copyfile(args.scanner, scan_dest)
        os.chmod(scan_dest, 0o755)
        pending.update(ldd_deps(args.scanner))

    # Recursively copy every non-excluded dependency into lib_dir.
    copied, n = set(), 0
    while pending:
        cur = sorted(pending)
        pending.clear()
        for src in cur:
            if src in copied:
                continue
            copied.add(src)
            if excluded(os.path.basename(src)) or not os.path.exists(src):
                continue
            if copy_lib(src, lib_dir):
                n += 1
            pending.update(d for d in ldd_deps(src) if d not in copied)

    print(f"linux gst bundle: {n} libs -> {lib_dir} "
          f"({len(os.listdir(plugin_out))} plugins/scanner in {plugin_out})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
