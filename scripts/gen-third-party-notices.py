#!/usr/bin/env python3
"""Generate THIRD-PARTY-NOTICES.txt for this crate's dependency tree.

Offline, dependency-free attribution generator, ported from the upstream punktfunk repo's
`scripts/gen-third-party-notices.py` and trimmed for this single-crate repo. It reads
`cargo metadata`, then for every third-party crate (everything that is not this crate itself)
pulls the crate's *actual* LICENSE/COPYING/NOTICE text out of the local cargo registry cache
(or the git checkout, for `punktfunk-core`), deduplicates identical license texts, and emits a
single notices file: a per-crate manifest followed by the verbatim license texts.

Unlike upstream this deliberately does NOT offer a `cargo about` path. The only build
environment this repo has is the ephemeral Docker container (see `Taskfile.yml`); installing
and running `cargo-about` there would add minutes to every regeneration for a
network-augmented result we don't need, and its output would not be reproducible offline.

The generated file is embedded into the binary (`src/ui/about.rs`) and shown on the app's
About & licenses screen, so it must be regenerated whenever the dependency tree changes:

    task notices

Usage:  python3 scripts/gen-third-party-notices.py [--out THIRD-PARTY-NOTICES.txt]
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys

LICENSE_GLOBS = ("license", "licence", "copying", "notice", "unlicense", "copyright")

# Non-Rust components shipped inside the `.ipk` or embedded in the binary. `cargo metadata`
# cannot see any of these, so they are listed explicitly: (label, what/why, license file
# relative to the repo root or None, source URL).
BUNDLED_COMPONENTS = [
    ("SDL2 (webosbrew/SDL-webOS backport)",
     "Bundled as lib/libSDL2-2.0.so.0 in the .ipk (the on-device system copy is too old — "
     "see docs/NOTES.md). Zlib license.",
     None,
     "https://github.com/webosbrew/SDL-webOS"),
    ("Geist (Geist Sans)",
     "punktfunk's brand typeface, embedded into the binary via include_bytes!. "
     "SIL Open Font License 1.1.",
     "assets/fonts/Geist-OFL.txt",
     "https://github.com/vercel/geist-font"),
    ("Material Icons (subset)",
     "Google's Material Icons, subsetted to the glyphs this UI draws and embedded via "
     "include_bytes!. Apache License 2.0.",
     "assets/icons/LICENSE",
     "https://github.com/google/material-design-icons"),
    ("NDL DirectMedia / Starfish (libplayerAPIs)",
     "LG webOS system libraries, linked at runtime from the device — NOT redistributed by "
     "this package. Header signatures were taken from mariotaku/ss4s.",
     None,
     "https://github.com/mariotaku/ss4s"),
]


def find_license_files(pkg_dir):
    out = []
    try:
        names = sorted(os.listdir(pkg_dir))
    except OSError:
        return out
    for n in names:
        low = n.lower()
        if any(low == g or low.startswith(g + ".") or low.startswith(g + "-") or g in low for g in LICENSE_GLOBS):
            p = os.path.join(pkg_dir, n)
            if os.path.isfile(p):
                try:
                    with open(p, "r", encoding="utf-8", errors="replace") as f:
                        txt = f.read().strip()
                    if txt:
                        out.append((n, txt))
                except OSError:
                    pass
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="THIRD-PARTY-NOTICES.txt")
    ap.add_argument("--manifest", default="Cargo.toml")
    ap.add_argument("--target", default="armv7-unknown-linux-gnueabi",
                    help="resolve the graph for this target only (the webOS cross target)")
    args = ap.parse_args()

    # `--filter-platform` restricts the resolved graph to what actually links for the webOS
    # target, so the notices file attributes what we really ship rather than every crate that
    # any platform's cfg could pull in (196 vs. the unfiltered superset). Deliberately NOT
    # `--offline`: the generator reads each crate's LICENSE file straight out of the registry
    # cache, so every package's source must be on disk anyway — letting cargo fetch a missing
    # one is strictly better than failing. `--offline` also fails outright on crates that are
    # in the lockfile but unused by this target (e.g. cpufeatures).
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1",
         "--filter-platform", args.target, "--manifest-path", args.manifest],
        text=True))
    ws_members = set(meta.get("workspace_members", []))

    pkgs = [p for p in meta["packages"] if p["id"] not in ws_members]
    pkgs.sort(key=lambda p: (p["name"].lower(), p["version"]))

    # Group license texts: text-hash -> {text, filename, crates[]}
    texts = {}
    no_text = []
    for p in pkgs:
        pkg_dir = os.path.dirname(p["manifest_path"])
        files = find_license_files(pkg_dir)
        label = f'{p["name"]} {p["version"]}'
        if not files:
            no_text.append(p)
            continue
        for fname, txt in files:
            h = hashlib.sha256(txt.encode("utf-8", "replace")).hexdigest()
            ent = texts.setdefault(h, {"text": txt, "filename": fname, "crates": set()})
            ent["crates"].add(label)

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    bundled = []
    for label, blurb, lic_path, url in BUNDLED_COMPONENTS:
        bundled.append((label, blurb, url))
        if lic_path is None:
            continue
        full = os.path.join(repo_root, lic_path)
        try:
            with open(full, encoding="utf-8", errors="replace") as f:
                txt = f.read().strip()
        except OSError:
            print(f"WARNING: bundled license missing: {lic_path}", file=sys.stderr)
            continue
        h = hashlib.sha256(txt.encode("utf-8", "replace")).hexdigest()
        ent = texts.setdefault(h, {"text": txt, "filename": os.path.basename(lic_path), "crates": set()})
        ent["crates"].add(label)

    lines = []
    w = lines.append
    w("THIRD-PARTY SOFTWARE NOTICES")
    w("=" * 76)
    w("")
    w("punktfunk-webos (https://github.com/dyptan-io/pf-webos) is licensed under")
    w("MIT OR Apache-2.0, matching upstream punktfunk (https://git.unom.io/unom/punktfunk).")
    w("The app links the third-party Rust crates listed below, and bundles or embeds the")
    w("non-Rust components listed first. Each is distributed under its own permissive")
    w("license; the full license texts follow the manifest.")
    w("")
    w("Generated by scripts/gen-third-party-notices.py — do not edit by hand.")
    w("")
    w(f"Total third-party crates: {len(pkgs)}")
    w("")
    if bundled:
        w("-" * 76)
        w("BUNDLED / EMBEDDED NON-RUST COMPONENTS")
        w("-" * 76)
        for label, blurb, url in bundled:
            w(f"  {label}")
            w(f"      {blurb}")
            w(f"      {url}")
        w("")
    w("-" * 76)
    w("MANIFEST (crate version — SPDX license — source)")
    w("-" * 76)
    for p in pkgs:
        lic = p.get("license") or (("file: " + p["license_file"]) if p.get("license_file") else "UNKNOWN")
        repo = p.get("repository") or ""
        w(f'  {p["name"]} {p["version"]} — {lic}' + (f' — {repo}' if repo else ""))
    w("")

    if no_text:
        w("-" * 76)
        w("Crates whose package did not embed a license file (SPDX + source only)")
        w("-" * 76)
        for p in no_text:
            lic = p.get("license") or "UNKNOWN"
            repo = p.get("repository") or ""
            w(f'  {p["name"]} {p["version"]} — {lic}' + (f' — {repo}' if repo else ""))
        w("")

    w("=" * 76)
    w("FULL LICENSE TEXTS (deduplicated)")
    w("=" * 76)
    # Stable order: by first crate name covered.
    for _h, ent in sorted(texts.items(), key=lambda kv: sorted(kv[1]["crates"])[0].lower()):
        crates = ", ".join(sorted(ent["crates"]))
        w("")
        w("-" * 76)
        w(f"The following license ({ent['filename']}) applies to: {crates}")
        w("-" * 76)
        w(ent["text"])
        w("")

    text = "\n".join(lines) + "\n"
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"wrote {args.out}: {len(pkgs)} crates, {len(texts)} distinct license texts, "
          f"{len(no_text)} without embedded text", file=sys.stderr)


if __name__ == "__main__":
    main()
