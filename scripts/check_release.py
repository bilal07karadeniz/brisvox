#!/usr/bin/env python3
"""Release consistency checks (standard library only; PyYAML is used when installed).

    python3 scripts/check_release.py                 # all checks (part of scripts/verify.sh)
    python3 scripts/check_release.py --tag v0.1.0    # also: the tag names the workspace version
    python3 scripts/check_release.py --notes v0.1.0  # print the CHANGELOG.md section of a tag
    python3 scripts/check_release.py --order         # print the crates.io publish order
    python3 scripts/check_release.py --version       # print the workspace version

What it guards (docs/RELEASING.md describes the release itself):
  * every publishable crate has the metadata crates.io shows, a README and both license files,
    and an exact `version` next to every intra-workspace `path` dependency (dev-dependencies
    carry none, so cargo drops them from the package and publish cycles cannot occur);
  * PUBLISH_ORDER is a valid dependency order of exactly the publishable crates;
  * the crates, the two Python packages and the three npm packages carry one version (the
    umbrella packages named plain `brisvox` pin the SDK packages they re-export to exactly it),
    CHANGELOG.md has an entry for it, and the license files of every package equal the
    repository's;
  * the `brisvox` crate forwards every feature of brisvox-server (except the binary's allocator);
  * the browser module embedded in brisvox-transport equals sdk/web/brisvox-call.js;
  * the workflows parse and pin every third-party action to a commit SHA.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Dependency order of the crates published to crates.io (a crate follows everything it depends on
# through normal, build or optional dependencies). brisvox-bench and the examples are never
# published. `cargo publish --dry-run --workspace` proves this order; this script keeps it honest.
PUBLISH_ORDER = [
    "brisvox-core",
    "brisvox-audio",
    "brisvox-inference",
    "brisvox-agent",
    "brisvox-providers",
    "brisvox-sim",
    "brisvox-transport",
    "brisvox-engine",
    "brisvox-agent-link",
    "brisvox-server",
    "brisvox",  # the umbrella crate: re-exports the ones above, so it goes last
]
LICENSES = ("LICENSE-MIT", "LICENSE-APACHE")
SDK_DIRS = ("sdk/python", "sdk/python-umbrella", "sdk/typescript", "sdk/web", "sdk/npm-umbrella")
WORKFLOWS = ("release.yml", "pages.yml", "ci.yml")

errors: list[str] = []


def fail(msg: str) -> None:
    errors.append(msg)


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    ).stdout
    return json.loads(out)


def same_bytes(a: str, b: str) -> bool:
    """True when two files hold the same text (line-ending differences of a checkout ignored)."""
    pa, pb = ROOT / a, ROOT / b
    if not pa.is_file() or not pb.is_file():
        return False
    return pa.read_bytes().replace(b"\r\n", b"\n") == pb.read_bytes().replace(b"\r\n", b"\n")


def check_crates(meta: dict) -> str:
    pkgs = {p["name"]: p for p in meta["packages"]}
    publishable = {n for n, p in pkgs.items() if p["publish"] != []}
    if publishable != set(PUBLISH_ORDER):
        fail(f"publishable crates {sorted(publishable)} differ from PUBLISH_ORDER {sorted(PUBLISH_ORDER)}")
    position = {n: i for i, n in enumerate(PUBLISH_ORDER)}
    versions = {p["version"] for p in pkgs.values()}
    if len(versions) != 1:
        fail(f"workspace crates carry several versions: {sorted(versions)}")
    for name in PUBLISH_ORDER:
        p = pkgs.get(name)
        if p is None:
            fail(f"{name}: not in the workspace")
            continue
        for field in ("description", "license", "repository", "homepage", "documentation", "readme", "rust_version"):
            if not p.get(field):
                fail(f"{name}: Cargo.toml has no `{field}`")
        if not 1 <= len(p["keywords"]) <= 5:
            fail(f"{name}: needs 1 to 5 keywords, has {len(p['keywords'])}")
        if not p["categories"]:
            fail(f"{name}: needs at least one category")
        crate_dir = Path(p["manifest_path"]).parent
        if not (crate_dir / "README.md").is_file():
            fail(f"{name}: README.md is missing")
        for lic in LICENSES:
            if not same_bytes(lic, str(crate_dir.relative_to(ROOT) / lic)):
                fail(f"{name}: {lic} is missing or differs from the repository's")
        for dep in p["dependencies"]:
            target = dep["name"]
            if target not in pkgs or not dep.get("path"):
                continue
            if dep["kind"] == "dev":
                if dep["req"] != "*":
                    fail(f"{name}: dev-dependency {target} must be path-only (no version): a versioned one blocks publishing")
                continue
            if dep["req"] == "*":
                fail(f"{name}: dependency {target} has a path but no version")
            elif dep["req"] != f"^{pkgs[target]['version']}":
                fail(f"{name}: dependency {target} requires {dep['req']}, expected ^{pkgs[target]['version']} (bump the workspace dependency version too)")
            if target not in position:
                fail(f"{name}: depends on {target}, which is not published")
            elif position[target] >= position[name]:
                fail(f"PUBLISH_ORDER: {target} must come before {name}")
    check_umbrella_features(pkgs)
    return next(iter(versions)) if versions else ""


def check_umbrella_features(pkgs: dict) -> None:
    """`brisvox` forwards each feature of brisvox-server by the same name (except the binary's own)."""
    umbrella, server = pkgs.get("brisvox"), pkgs.get("brisvox-server")
    if umbrella is None or server is None:
        return
    own = {"default", "mimalloc"}  # mimalloc is the allocator of the server's binary only
    for feature in sorted(set(server["features"]) - own):
        want = f"brisvox-server/{feature}"
        if want not in umbrella["features"].get(feature, []):
            fail(f"brisvox: feature `{feature}` must forward to `{want}`")
    for feature in sorted(set(umbrella["features"]) - {"default"}):
        if feature not in server["features"]:
            fail(f"brisvox: feature `{feature}` does not exist in brisvox-server")
    server_dep = next((d for d in umbrella["dependencies"] if d["name"] == "brisvox-server"), None)
    if server_dep is None or server_dep.get("uses_default_features", True):
        fail("brisvox: the brisvox-server dependency must set default-features = false (the umbrella's own default list decides)")


def check_sdks(version: str) -> None:
    py = read("sdk/python/pyproject.toml")
    m = re.search(r'^version = "([^"]+)"', py, re.M)
    if not m or m.group(1) != version:
        fail(f"sdk/python/pyproject.toml: version {m.group(1) if m else None} != {version}")
    for key in ("license-files", "[project.urls]", "readme"):
        if key not in py:
            fail(f"sdk/python/pyproject.toml: `{key}` is missing")
    for d in SDK_DIRS:
        for lic in LICENSES:
            if not same_bytes(lic, f"{d}/{lic}"):
                fail(f"{d}: {lic} is missing or differs from the repository's")
    for d in ("sdk/typescript", "sdk/web", "sdk/npm-umbrella"):
        pj = json.loads(read(f"{d}/package.json"))
        if pj.get("version") != version:
            fail(f"{d}/package.json: version {pj.get('version')} != {version}")
        for field in ("name", "description", "license", "repository", "homepage", "bugs", "keywords", "files"):
            if not pj.get(field):
                fail(f"{d}/package.json: `{field}` is missing")
        if (pj.get("repository") or {}).get("directory") != d:
            fail(f"{d}/package.json: repository.directory must be {d}")
        if (pj.get("publishConfig") or {}).get("access") != "public":
            fail(f"{d}/package.json: publishConfig.access must be public")
        for lic in LICENSES:
            if lic not in pj.get("files", []):
                fail(f"{d}/package.json: `files` must list {lic}")


def check_sdk_constants(version: str) -> None:
    ts = read("sdk/typescript/src/index.ts")
    m = re.search(r'^export const VERSION = "([^"]+)";', ts, re.M)
    if not m or m.group(1) != version:
        fail(f"sdk/typescript/src/index.ts: VERSION {m.group(1) if m else None} != {version}")


def check_umbrellas(version: str) -> None:
    """The packages named plain `brisvox` on PyPI and npm re-export the SDKs and pin them exactly."""
    py = read("sdk/python-umbrella/pyproject.toml")
    m = re.search(r'^name = "([^"]+)"', py, re.M)
    if not m or m.group(1) != "brisvox":
        fail(f"sdk/python-umbrella/pyproject.toml: name {m.group(1) if m else None} != brisvox")
    m = re.search(r'^version = "([^"]+)"', py, re.M)
    if not m or m.group(1) != version:
        fail(f"sdk/python-umbrella/pyproject.toml: version {m.group(1) if m else None} != {version}")
    if f'"brisvox-agent=={version}"' not in py:
        fail(f"sdk/python-umbrella/pyproject.toml: must depend on brisvox-agent=={version} exactly")
    for key in ("license-files", "[project.urls]", "readme"):
        if key not in py:
            fail(f"sdk/python-umbrella/pyproject.toml: `{key}` is missing")
    init = read("sdk/python-umbrella/brisvox/__init__.py")
    m = re.search(r'^__version__ = "([^"]+)"', init, re.M)
    if not m or m.group(1) != version:
        fail(f"sdk/python-umbrella/brisvox/__init__.py: __version__ {m.group(1) if m else None} != {version}")
    sdk_init = read("sdk/python/brisvox_agent/__init__.py")
    m = re.search(r'^__version__ = "([^"]+)"', sdk_init, re.M)
    if not m or m.group(1) != version:
        fail(f"sdk/python/brisvox_agent/__init__.py: __version__ {m.group(1) if m else None} != {version}")

    pj = json.loads(read("sdk/npm-umbrella/package.json"))
    if pj.get("name") != "brisvox":
        fail(f"sdk/npm-umbrella/package.json: name {pj.get('name')} != brisvox")
    for dep in ("@brisvox/agent", "@brisvox/web"):
        if (pj.get("dependencies") or {}).get(dep) != version:
            fail(f"sdk/npm-umbrella/package.json: dependency {dep} must be pinned to exactly {version}")
    if "./web" not in (pj.get("exports") or {}):
        fail('sdk/npm-umbrella/package.json: `exports` needs "./web" (the browser entry)')


def check_changelog(version: str) -> None:
    text = read("CHANGELOG.md")
    if not re.search(rf"^## \[{re.escape(version)}\]", text, re.M):
        fail(f"CHANGELOG.md has no `## [{version}]` entry")
    if "## [Unreleased]" not in text:
        fail("CHANGELOG.md has no `## [Unreleased]` section")


def notes(tag: str) -> str:
    version = tag[1:] if tag.startswith("v") else tag
    text = read("CHANGELOG.md").replace("\r\n", "\n")
    m = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|^\[[^\]]+\]: |\Z)", text, re.M | re.S)
    if not m or not m.group(1).strip():
        raise SystemExit(f"CHANGELOG.md has no notes for {version}")
    return m.group(1).strip() + "\n"


def check_embedded_module() -> None:
    if not same_bytes("sdk/web/brisvox-call.js", "crates/brisvox-transport/src/webrtc/brisvox-call.js"):
        fail(
            "crates/brisvox-transport/src/webrtc/brisvox-call.js differs from sdk/web/brisvox-call.js "
            "(copy the sdk file over it)"
        )


def check_workflows() -> None:
    try:
        import yaml
    except ImportError:
        yaml = None
        print("check_release: PyYAML not installed, workflow YAML syntax not parsed")
    for name in WORKFLOWS:
        path = ROOT / ".github" / "workflows" / name
        if not path.is_file():
            fail(f".github/workflows/{name} is missing")
            continue
        text = path.read_text(encoding="utf-8")
        if yaml is not None:
            try:
                doc = yaml.safe_load(text)
            except yaml.YAMLError as e:
                fail(f"{name}: invalid YAML: {e}")
                continue
            if not isinstance(doc, dict) or "jobs" not in doc or (True not in doc and "on" not in doc):
                fail(f"{name}: needs `on` and `jobs`")
            if "permissions" not in doc:
                fail(f"{name}: needs a top-level `permissions` (least privilege; jobs widen it)")
        for m in re.finditer(r"^\s*(?:-\s*)?uses:\s*(\S+)", text, re.M):
            ref = m.group(1)
            if ref.startswith("./"):
                continue
            if not re.fullmatch(r"[\w.-]+/[\w./-]+@[0-9a-f]{40}", ref):
                fail(f"{name}: `uses: {ref}` is not pinned to a 40-character commit SHA")
    release = ROOT / ".github" / "workflows" / "release.yml"
    if release.is_file() and "'v*'" not in release.read_text(encoding="utf-8") and '"v*"' not in release.read_text(encoding="utf-8"):
        fail("release.yml must trigger on `v*` tags")


def main(argv: list[str]) -> int:
    if "--notes" in argv:
        sys.stdout.write(notes(argv[argv.index("--notes") + 1]))
        return 0
    if "--order" in argv:
        print("\n".join(PUBLISH_ORDER))
        return 0
    meta = metadata()
    if "--version" in argv:
        print(next(p["version"] for p in meta["packages"] if p["name"] == "brisvox-core"))
        return 0
    version = check_crates(meta)
    check_sdks(version)
    check_sdk_constants(version)
    check_umbrellas(version)
    check_changelog(version)
    check_embedded_module()
    check_workflows()
    if "--tag" in argv:
        tag = argv[argv.index("--tag") + 1]
        if tag != f"v{version}":
            fail(f"tag {tag} does not match the workspace version v{version}")
        try:
            notes(tag)
        except SystemExit as e:
            fail(str(e))
    if errors:
        print("check_release: FAILED")
        for e in errors:
            print(f"  - {e}")
        return 1
    print(f"check_release: ok (version {version}, {len(PUBLISH_ORDER)} crates, 2 python and 3 npm packages)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
