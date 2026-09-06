#!/usr/bin/env python3
"""Render assets/logo.svg into PNGs + a multi-resolution icon.ico.

Renderer: any preinstalled Chromium (Chrome/Edge) or Firefox, headless
screenshot at exact sizes (SVG scales crisply). ICO packing is pure
stdlib (PNG-compressed entries). No pip packages needed.

Usage:
    python3 tools/render_icons.py --svg assets/logo.svg --outdir <dir>
Writes: icon-16/32/48/128/256.png + icon.ico into <outdir>.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile

SIZES = (16, 32, 48, 128, 256)


def find_browsers():
    """Candidate (name, argv-prefix) pairs per platform."""
    cands = []
    if sys.platform == "win32":
        pf = os.environ.get("ProgramFiles", r"C:\Program Files")
        pfx86 = os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")
        cands += [
            ("edge", [os.path.join(pf, "Microsoft", "Edge", "Application", "msedge.exe")]),
            ("chrome", [os.path.join(pf, "Google", "Chrome", "Application", "chrome.exe")]),
            ("chrome-x86", [os.path.join(pfx86, "Google", "Chrome", "Application", "chrome.exe")]),
        ]
    elif sys.platform == "darwin":
        cands += [
            ("chrome-mac", ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]),
            ("edge-mac", ["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"]),
        ]
    else:
        cands += [
            ("google-chrome", ["google-chrome"]),
            ("chromium", ["chromium"]),
            ("chromium-browser", ["chromium-browser"]),
            ("firefox", ["firefox"]),
        ]
    out = []
    for name, argv in cands:
        exe = argv[0]
        if os.path.isabs(exe):
            if os.path.isfile(exe):
                out.append((name, argv))
        elif shutil.which(exe):
            out.append((name, argv))
    return out


def screenshot(browser_argv, name, svg_path, size, dest):
    """Headless screenshot of the SVG at exactly size x size."""
    if name == "firefox":
        cmd = browser_argv + [
            "--headless",
            "--screenshot", dest,
            f"--window-size={size},{size}",
            svg_path.as_uri() if hasattr(svg_path, "as_uri") else path_uri(svg_path),
        ]
    else:
        cmd = browser_argv + [
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            f"--window-size={size},{size}",
            f"--screenshot={dest}",
            path_uri(svg_path),
        ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if not os.path.isfile(dest) or os.path.getsize(dest) == 0:
        raise RuntimeError(
            f"{name} screenshot failed (size {size}): {proc.stderr[-2000:]}"
        )


def path_uri(path):
    from pathlib import Path
    return Path(os.path.abspath(path)).as_uri()


def pack_ico(png_paths, ico_path):
    """Multi-image ICO with PNG-compressed entries (Vista+)."""
    blobs = []
    for path in png_paths:
        with open(path, "rb") as handle:
            data = handle.read()
        if data[:8] != b"\x89PNG\r\n\x1a\n":
            raise RuntimeError(f"not a PNG: {path}")
        width = struct.unpack(">I", data[16:20])[0]
        blobs.append((width, data))
    header = struct.pack("<HHH", 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    entries = b""
    for width, data in blobs:
        dim = 0 if width >= 256 else width
        entries += struct.pack(
            "<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset
        )
        offset += len(data)
    with open(ico_path, "wb") as handle:
        handle.write(header + entries)
        for _, data in blobs:
            handle.write(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--svg", required=True)
    parser.add_argument("--outdir", required=True)
    args = parser.parse_args()

    if not os.path.isfile(args.svg):
        print(f"missing svg: {args.svg}", file=sys.stderr)
        return 2
    os.makedirs(args.outdir, exist_ok=True)

    browsers = find_browsers()
    if not browsers:
        print("no supported browser found (need Chrome/Edge/Firefox)", file=sys.stderr)
        return 3
    print("browser candidates:", ", ".join(name for name, _ in browsers))

    pngs = []
    errors = []
    for name, argv in browsers:
        try:
            with tempfile.TemporaryDirectory() as tmp:
                trial = []
                for size in SIZES:
                    dest = os.path.join(tmp, f"icon-{size}.png")
                    screenshot(argv, name, args.svg, size, dest)
                    trial.append(dest)
                # All sizes rendered: keep this browser's output.
                for size, src in zip(SIZES, trial):
                    final = os.path.join(args.outdir, f"icon-{size}.png")
                    shutil.copyfile(src, final)
                    print(f"rendered {final}")
                pngs = [os.path.join(args.outdir, f"icon-{s}.png") for s in SIZES]
                break
        except Exception as exc:  # try next browser
            errors.append(f"{name}: {exc}")
    if not pngs:
        print("all browsers failed:\n" + "\n".join(errors), file=sys.stderr)
        return 4

    ico_path = os.path.join(args.outdir, "icon.ico")
    # ICO takes 16/32/48/128/256 (skip nothing; struct handles 256 as 0).
    pack_ico([os.path.join(args.outdir, f"icon-{s}.png") for s in (16, 32, 48, 128, 256)], ico_path)
    print(f"packed {ico_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
