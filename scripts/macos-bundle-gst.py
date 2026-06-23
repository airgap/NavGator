#!/usr/bin/env python3
"""Bundle GStreamer + every non-system dylib dependency into a macOS .app so it runs
without Homebrew installed.

Ported from servo's python/servo/gstreamer.py::package_gstreamer_dylibs. NavGator builds
with `cargo build -p navgator`, which skips servo's mach post-build packaging — so the
binary ships linking GStreamer/glib by absolute /opt/homebrew paths and crashes at launch
on any machine without them ("Library not loaded: …/libgstplay-1.0.0.dylib").

libservo (servo.rs media init) loads its curated GStreamer plugins from <exe_dir>/lib at
runtime, so we copy the binary's non-system deps + the curated plugin list (+ their
transitive deps) recursively into <exe_dir>/lib and rewrite every install name to
@executable_path/lib/. Run BEFORE codesign so `codesign --deep` seals the bundled dylibs.
"""
import argparse
import ast
import os
import shutil
import subprocess
import sys

# Fallback if the swervo plugin lists can't be located (keep in sync with
# components/servo/gstreamer_plugin_lists/{common,macos}.rs.in).
FALLBACK_PLUGINS = [
    "gstcoreelements", "gstnice", "gstapp", "gstaudioconvert", "gstaudioresample",
    "gstgio", "gstogg", "gstopengl", "gstopus", "gstplayback", "gsttheora",
    "gsttypefindfunctions", "gstvideoconvertscale", "gstvolume", "gstvorbis",
    "gstaudiofx", "gstaudioparsers", "gstautodetect", "gstdeinterlace", "gstid3demux",
    "gstinterleave", "gstisomp4", "gstmatroska", "gstrtp", "gstrtpmanager",
    "gstvideofilter", "gstvpx", "gstwavparse", "gstaudiobuffersplit", "gstdtls",
    "gstid3tag", "gstproxy", "gstvideoparsersbad", "gstwebrtc", "gstlibav",
    "gstosxaudio", "gstosxvideo", "gstapplemedia",
]


def is_system_lib(path):
    return path.startswith("/System/Library") or path.startswith("/usr/lib") or ".asan." in path


def otool_deps(binary):
    """Non-system dylib dependency paths of a Mach-O, via `otool -L`."""
    out = set()
    res = subprocess.run(["/usr/bin/otool", "-L", binary], capture_output=True, text=True)
    for line in res.stdout.splitlines():
        if not line.startswith("\t"):
            continue
        dep = line.split(" ", 1)[0].strip()
        if dep and not is_system_lib(dep) and "librustc" not in dep:
            out.add(dep)
    return out


def rewrite_relative(binary, deps, rel):
    """install_name_tool -change each dep to @executable_path/<rel>/<basename>."""
    for dep in deps:
        if is_system_lib(dep) or dep.startswith("@rpath/") or dep.startswith("@executable_path/"):
            continue
        new = os.path.join("@executable_path", rel, os.path.basename(dep))
        subprocess.run(["install_name_tool", "-change", dep, new, binary],
                       check=False, capture_output=True)


def rpath_to_abs(dep, gst_libs):
    """Resolve an @rpath/ dependency to a real path under the gstreamer lib root."""
    if not dep.startswith("@rpath/"):
        return dep
    relp = dep[len("@rpath/"):]
    for d in ("", "..", "gstreamer-1.0"):
        full = os.path.join(gst_libs, d, relp)
        if os.path.exists(full):
            return os.path.normpath(full)
    return None


def load_plugins(plugin_lists_dir):
    names = []
    if plugin_lists_dir and os.path.isdir(plugin_lists_dir):
        for fn in ("common.rs.in", "macos.rs.in"):
            fp = os.path.join(plugin_lists_dir, fn)
            if os.path.exists(fp):
                stripped = [ln.strip() for ln in open(fp) if not ln.strip().startswith("//")]
                try:
                    names += ast.literal_eval(" ".join(stripped))
                except (SyntaxError, ValueError):
                    pass
    if not names:
        print("macOS GStreamer bundle: using built-in fallback plugin list", file=sys.stderr)
        names = FALLBACK_PLUGINS
    return [f"lib{n}.dylib" for n in names]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True, help="path to the Mach-O executable in the .app")
    ap.add_argument("--lib-dir", required=True, help="output dir, e.g. NavGator.app/Contents/MacOS/lib")
    ap.add_argument("--gst-libs", required=True, help="gstreamer lib root, e.g. /opt/homebrew/lib")
    ap.add_argument("--plugin-lists", default="", help="swervo gstreamer_plugin_lists dir (optional)")
    args = ap.parse_args()

    rel = os.path.relpath(args.lib_dir, os.path.dirname(args.binary)) + "/"
    os.makedirs(args.lib_dir, exist_ok=True)

    plugin_dir = os.path.join(args.gst_libs, "gstreamer-1.0")
    deps = otool_deps(args.binary)
    missing = []
    for plugin in load_plugins(args.plugin_lists):
        pp = os.path.join(plugin_dir, plugin)
        (deps.add(pp) if os.path.exists(pp) else missing.append(plugin))
    if missing:
        print("macOS GStreamer bundle: WARN plugins not found: " + ", ".join(missing), file=sys.stderr)

    # The binary's own GStreamer/glib links -> @executable_path/lib/
    rewrite_relative(args.binary, deps, rel)

    copied, pending, n = set(), set(deps), 0
    while pending:
        cur = set(pending)
        pending.clear()
        for dep in cur:
            copied.add(dep)
            src = rpath_to_abs(dep, args.gst_libs)
            if not src or not os.path.exists(src):
                print(f"macOS GStreamer bundle: WARN cannot resolve {dep}", file=sys.stderr)
                continue
            transitive = otool_deps(src)
            new_path = os.path.join(args.lib_dir, os.path.basename(src))
            if not os.path.exists(new_path):
                n += 1
                shutil.copyfile(src, new_path)
                os.chmod(new_path, 0o644)
                rewrite_relative(new_path, transitive, rel)
            pending.update(transitive - copied)

    print(f"macOS GStreamer bundle: {n} dylibs -> {args.lib_dir} (@executable_path/{rel})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
