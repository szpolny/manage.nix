"""Select VM coverage conservatively; unknown paths run every core scenario."""

import json
import os
import subprocess
import tomllib

CORE = {"vm-activate", "vm-deploy-sqlite", "vm-web", "vm-fleet", "vm-local-https"}
ALL = CORE | {
    "vm-hello",
    "vm-enroll-sqlite",
    "vm-enroll-postgres",
    "vm-deploy-postgres",
}
VERSION_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "web/package.json",
    "web/package-lock.json",
    ".release-please-manifest.json",
    "version.txt",
}


def without_version(path, content):
    """Remove only release-managed version fields, preserving dependency data."""
    if path == "version.txt":
        return None
    data = (
        tomllib.loads(content)
        if path.endswith((".toml", ".lock"))
        else json.loads(content)
    )
    if path == "Cargo.toml":
        del data["workspace"]["package"]["version"]
    elif path == "Cargo.lock":
        for package in data["package"]:
            if package["name"].startswith("mnix-") and "source" not in package:
                del package["version"]
    elif path == ".release-please-manifest.json":
        del data["."]
    else:
        del data["version"]
        if path == "web/package-lock.json":
            del data["packages"][""]["version"]
    return data


def version_only(path, base, head):
    if path not in VERSION_FILES or not base or not head:
        return False
    try:
        before, after = (
            subprocess.check_output(["git", "show", f"{ref}:{path}"], text=True)
            for ref in (base, head)
        )
        return without_version(path, before) == without_version(path, after)
    except (subprocess.CalledProcessError, ValueError, KeyError, TypeError):
        # Missing files or unexpected formats must not reduce coverage.
        return False


def select(paths, base=None, head=None):
    tests = set()
    for path in paths:
        if version_only(path, base, head):
            continue
        if path.startswith(("web/", "nix/tests/web/")):
            tests.add("vm-web")
        elif path == "nix/tests/local-https.nix":
            tests.add("vm-local-https")
        elif path == "nix/tests/hello.nix":
            tests.add("vm-hello")
        elif path == "nix/tests/activate.nix":
            tests.add("vm-activate")
        elif path == "nix/tests/enroll.nix":
            tests.update({"vm-enroll-sqlite", "vm-enroll-postgres"})
        elif path == "nix/tests/fleet.nix":
            tests.add("vm-fleet")
        elif path == "nix/tests/deploy.nix":
            tests.update({"vm-deploy-sqlite", "vm-deploy-postgres", "vm-web"})
        elif path.startswith("nix/tests/deploy/"):
            tests.update(
                {"vm-deploy-sqlite", "vm-deploy-postgres", "vm-web", "vm-fleet"}
            )
        elif path.startswith("docs/") or path.endswith(".md"):
            continue
        else:
            tests.update(CORE)
    return sorted(tests)


def main():
    event = os.environ["EVENT_NAME"]
    if event in {"schedule", "workflow_dispatch"}:
        tests = sorted(ALL)
    else:
        base = os.environ["BASE_SHA"]
        head = os.environ["HEAD_SHA"]
        try:
            if not base or set(base) == {"0"}:
                raise ValueError("No previous commit")
            if event == "pull_request":
                base = subprocess.check_output(
                    ["git", "merge-base", base, head], text=True
                ).strip()
            paths = (
                subprocess.check_output(
                    ["git", "diff", "--name-only", "--no-renames", "-z", base, head]
                )
                .decode()
                .split("\0")
            )
            tests = select((path for path in paths if path), base, head)
        except (ValueError, subprocess.CalledProcessError):
            tests = sorted(CORE)
    print("Selected VM tests:", ", ".join(tests) or "none")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        # Keep the matrix valid even when the entire job is skipped.
        output.write(f"tests={json.dumps(tests or ['vm-hello'])}\n")
        output.write(f"has-tests={str(bool(tests)).lower()}\n")


if __name__ == "__main__":
    main()
