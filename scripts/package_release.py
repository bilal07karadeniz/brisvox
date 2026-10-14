#!/usr/bin/env python3
"""Pack a built brisvox-server binary into the release archive and write its checksum.

    python3 scripts/package_release.py --version v0.1.0 --target x86_64-unknown-linux-gnu \
        --binary target/release/brisvox-server --out dist

Writes `dist/brisvox-server-<version>-<target>.tar.gz` (`.zip` for a Windows target) and
`<archive>.sha256` (the `sha256sum` format). The archive holds one folder with the binary,
README.txt, the license files, the model notices, the reference agent definitions and
scripts/models/fetch_models.py (the Smart Turn model is downloaded by the user, not shipped).
Standard library only; the layout is described in deploy/release-readme.txt.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def entries(version: str, target: str, binary: Path) -> list[tuple[Path | bytes, str, int]]:
    """(source file or bytes, name inside the folder, unix mode)."""
    exe = "brisvox-server.exe" if "windows" in target else "brisvox-server"
    readme = (ROOT / "deploy/release-readme.txt").read_text(encoding="utf-8")
    readme = readme.replace("@VERSION@", version).replace("@TARGET@", target)
    out: list[tuple[Path | bytes, str, int]] = [
        (binary, exe, 0o755),
        (readme.replace("\r\n", "\n").encode("utf-8"), "README.txt", 0o644),
        (ROOT / "LICENSE-MIT", "LICENSE-MIT", 0o644),
        (ROOT / "LICENSE-APACHE", "LICENSE-APACHE", 0o644),
        (ROOT / "deploy/models-notice.txt", "NOTICE-models.txt", 0o644),
        (ROOT / "scripts/models/fetch_models.py", "scripts/models/fetch_models.py", 0o755),
    ]
    for agent in sorted((ROOT / "deploy/agents").glob("*.toml")):
        out.append((agent, f"agents/{agent.name}", 0o644))
    return out


def read_bytes(src: Path | bytes) -> bytes:
    return src if isinstance(src, bytes) else src.read_bytes()


def write_tar(path: Path, folder: str, items) -> None:
    with tarfile.open(path, "w:gz", compresslevel=9) as tar:
        for src, name, mode in items:
            data = read_bytes(src)
            if name.endswith((".txt", ".toml", ".py")) or name.startswith("LICENSE"):
                data = data.replace(b"\r\n", b"\n")
            info = tarfile.TarInfo(f"{folder}/{name}")
            info.size = len(data)
            info.mode = mode
            info.mtime = 0
            info.uname = info.gname = ""
            tar.addfile(info, io.BytesIO(data))


def write_zip(path: Path, folder: str, items) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for src, name, _mode in items:
            data = read_bytes(src)
            if name.endswith((".txt", ".toml", ".py")) or name.startswith("LICENSE"):
                data = data.replace(b"\r\n", b"\n")
            info = zipfile.ZipInfo(f"{folder}/{name}", date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--version", required=True, help="tag, for example v0.1.0")
    ap.add_argument("--target", required=True, help="Rust target triple")
    ap.add_argument("--binary", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()
    if not args.binary.is_file():
        raise SystemExit(f"binary not found: {args.binary}")
    args.out.mkdir(parents=True, exist_ok=True)
    folder = f"brisvox-server-{args.version}-{args.target}"
    items = entries(args.version, args.target, args.binary)
    if "windows" in args.target:
        archive = args.out / f"{folder}.zip"
        write_zip(archive, folder, items)
    else:
        archive = args.out / f"{folder}.tar.gz"
        write_tar(archive, folder, items)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (args.out / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8", newline="\n")
    print(f"{archive} {digest}")


if __name__ == "__main__":
    main()
