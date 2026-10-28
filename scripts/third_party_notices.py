#!/usr/bin/env python3
"""Collect original notices from the locked Cargo graph, without machine paths."""
import json
import argparse
from pathlib import Path
import subprocess

root=Path(__file__).resolve().parents[1]
metadata=json.loads(subprocess.check_output(["cargo","metadata","--locked","--format-version","1"],cwd=root,text=True))
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check",action="store_true",help="verify the checked-in bundle is current")
args=parser.parse_args()
overrides=json.loads((root/"scripts/license-overrides.json").read_text())
sections=["Crow Term — third-party Rust dependency licenses and notices\n\n"
          "Collected from the Cargo.lock graph, including target/build/test dependencies.\n"
          "This is a conservative superset, not a claim that every package is linked\n"
          "into every platform binary. Original notices below are unmodified.\n"
          "Two upstream packages omit original license files; their exact source\n"
          "declarations and a separately labelled MIT template are included.\n"
          "Font/theme/plugin notices are distributed separately in assets/ and shell/.\n"]
missing=[]
count=0
for package in sorted(metadata["packages"],key=lambda p:(p["name"],p["version"])):
    if not package["source"]:
        continue
    directory=Path(package["manifest_path"]).parent
    # Crates can vendor separate Unicode/protocol/data licenses below src/ or
    # protocol directories. A root-only scan silently drops those notices.
    files={p for p in directory.rglob("*") if p.is_file() and p.name.lower().startswith(("license","licence","copying","copyright","notice"))}
    if package.get("license_file"):
        path=directory/package["license_file"]
        if path.is_file():files.add(path)
    for folder in [directory/"LICENSES",directory/"licenses"]:
        if folder.is_dir():files.update(p for p in folder.rglob("*") if p.is_file())
    if not files:
        override=overrides.get(package["name"]+" "+package["version"])
        if not override:
            missing.append(package["name"]+" "+package["version"])
            continue
        files={root/path for path in override["files"]}
        if any(not path.is_file() or not path.resolve().is_relative_to(root) for path in files):
            raise SystemExit("Invalid cached upstream notice path")
    else:
        override=None
    sections.append("\n"+"="*78+"\n"+package["name"]+" "+package["version"]+"\nSPDX: "+str(package.get("license") or "see original files")+"\nSource: https://crates.io/crates/"+package["name"]+"/"+package["version"]+"\n")
    if override:
        sections.append("Cached immutable upstream sources:\n"+"\n".join(override["sources"])+"\n")
        if override.get("note"):
            sections.append(override["note"]+"\n")
    for path in sorted(files):
        label=path.relative_to(directory) if path.is_relative_to(directory) else path.relative_to(root)
        sections.append("\n--- "+str(label)+" ---\n"+path.read_bytes().decode("utf-8")+"\n")
    count+=1
if missing:
    raise SystemExit("Missing original license files: "+", ".join(missing))
output="\n".join(sections)
if str(Path.home()) in output:
    raise SystemExit("Refusing a notice bundle containing a local home path")
destination=root/"THIRD_PARTY_LICENSES.txt"
if args.check:
    if not destination.is_file() or destination.read_bytes().decode("utf-8")!=output:
        raise SystemExit("THIRD_PARTY_LICENSES.txt is stale; run scripts/third_party_notices.py")
else:
    destination.write_bytes(output.encode("utf-8"))
print(f"Collected original notices for {count} packages")
