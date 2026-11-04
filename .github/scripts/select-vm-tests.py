"""Select VM coverage conservatively; unknown paths run every core scenario."""

import json
import os
import subprocess

CORE = {"vm-activate", "vm-deploy-sqlite", "vm-web", "vm-fleet"}
ALL = CORE | {
    "vm-hello", "vm-enroll-sqlite", "vm-enroll-postgres", "vm-deploy-postgres"
}


def select(paths):
    tests = set()
    for path in paths:
        if path.startswith("web/") or path.startswith("nix/tests/web/"):
            tests.add("vm-web")
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
            tests.update({"vm-deploy-sqlite", "vm-deploy-postgres", "vm-web", "vm-fleet"})
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
            paths = subprocess.check_output(
                ["git", "diff", "--name-only", "--no-renames", "-z", base, head]
            ).decode().split("\0")
            tests = select(path for path in paths if path)
        except (ValueError, subprocess.CalledProcessError):
            tests = sorted(CORE)
    print("Selected VM tests:", ", ".join(tests) or "none")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        # Keep the matrix valid even when the entire job is skipped.
        output.write(f"tests={json.dumps(tests or ['vm-hello'])}\n")
        output.write(f"has-tests={str(bool(tests)).lower()}\n")


if __name__ == "__main__":
    main()
