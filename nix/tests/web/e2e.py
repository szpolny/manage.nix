"""Drives the manage.nix web UI in Chromium for the `web` VM test.

Each command logs in as `admin`, does one step the way a person would, and
prints what the test script needs:

    e2e.py enroll <machine>           add a machine; print its enrollment token
    e2e.py configure <machine> <repo> wait until it is online; build it from <repo>
    e2e.py deploy <machine> <rev>     deploy <rev> and follow it to the end
    e2e.py verify <machine> <rev>     the machine runs <rev>; diff its generations

On failure it saves a screenshot to /tmp/e2e/<command>.png.
"""

import os
import re
import sys

from playwright.sync_api import Page, expect, sync_playwright

BASE = os.environ.get("MNIX_URL", "http://server:8080")
PASSWORD = open(os.environ["MNIX_PASSWORD_FILE"]).read().strip()
# Building and activating in small VMs is slow.
DEPLOY_TIMEOUT = 15 * 60 * 1000


def login(page: Page) -> None:
    page.goto(f"{BASE}/")
    page.wait_for_url(re.compile(r"/login"))
    username = page.get_by_label("Username")
    password = page.get_by_label("Password")
    username.fill("admin")
    password.fill(PASSWORD)
    expect(username).to_have_value("admin")
    page.get_by_role("button", name="Log in").click()
    expect(page.get_by_role("heading", name="Machines")).to_be_visible(timeout=30_000)
    expect(page.get_by_role("link", name="admin")).to_be_visible()


def enroll(page: Page, machine: str) -> None:
    page.get_by_role("button", name="Add machine").first.click()
    dialog = page.get_by_role("dialog", name="Add machine")
    dialog.get_by_label("Name").fill(machine)
    dialog.get_by_role("button", name="Add").click()
    token = dialog.get_by_test_id("enrollment-token")
    expect(token).to_have_text(re.compile(r"^mnix_enroll_"))
    print(token.inner_text().strip())


def configure(page: Page, machine: str, repository: str) -> None:
    page.goto(f"{BASE}/machines/{machine}")
    header = page.locator(".page-header [data-status]")
    # The agent enrolls and connects; the page refreshes every 5 seconds.
    expect(header).to_have_attribute("data-status", "online", timeout=120_000)
    page.get_by_label("Repository").select_option(repository)
    page.get_by_role("button", name="Save", exact=True).click()
    expect(page.get_by_role("status")).to_have_text("Saved configuration.")
    # The running system was not built by manage.nix.
    expect(page.get_by_test_id("revision")).to_have_text("not built by manage.nix")


def deploy(page: Page, machine: str, rev: str) -> None:
    page.get_by_role("checkbox", name=f"Select {machine}").check()
    page.get_by_role("button", name="Deploy 1 selected").click()
    dialog = page.get_by_role("dialog", name="Deploy")
    dialog.get_by_label("Revision").fill(rev)
    expect(dialog.get_by_role("checkbox", name=re.compile(machine))).to_be_checked()
    dialog.get_by_role("button", name="Deploy 1 machine").click()
    page.wait_for_url(re.compile(r"/deploys/[0-9a-f-]{36}$"))
    deployment = page.url.rsplit("/", 1)[1]
    status = page.locator(".page-header [data-status]")
    page.wait_for_function(
        "() => ['succeeded', 'failed'].includes("
        "document.querySelector('.page-header [data-status]')?.dataset.status)",
        timeout=DEPLOY_TIMEOUT,
    )
    log = page.get_by_test_id("log")
    print(log.inner_text(), file=sys.stderr)
    expect(status).to_have_attribute("data-status", "succeeded")
    # The log streamed in while the deployment ran.
    expect(log).to_contain_text("the server confirmed the new system")
    expect(page.locator(f"tr[data-machine='{machine}'] [data-status]")).to_have_attribute(
        "data-status", "succeeded"
    )
    expect(page.get_by_text("Requested by")).to_be_visible()
    print(deployment)


def verify(page: Page, machine: str, rev: str) -> None:
    # The machines list shows the revision the machine runs.
    row = page.locator(f"tr[data-machine='{machine}']")
    # A newer commit on the tracked ref adds "N behind <ref>" after it.
    expect(row.get_by_test_id("revision")).to_have_text(
        re.compile(rf"^{rev} @ [0-9a-f]{{8}}( \d+ behind \S+)?$")
    )
    expect(row.locator("[data-status]").first).to_have_attribute("data-status", "online")

    row.get_by_role("link", name=machine).click()
    expect(page.get_by_test_id("revision")).to_contain_text(f"{rev} @")
    expect(page.get_by_role("heading", name="Recent deploys")).to_be_visible()
    expect(page.locator("section", has_text="Recent deploys").locator("[data-status]")).to_have_attribute(
        "data-status", "succeeded"
    )

    # Compare the system the machine booted (generation 1) with the deployed
    # one (generation 2), which adds the revision's demo service.
    page.get_by_role("radio", name="Compare from generation 1").check()
    page.get_by_role("radio", name="Compare to generation 2").check()
    page.get_by_role("button", name="Compare 1 → 2").click()
    diff = page.get_by_test_id("diff")
    expect(diff).to_be_visible(timeout=120_000)
    text = diff.inner_text()
    print(text)
    assert "mnix-test" in text or "→" in text, text

    # The deploys page lists it.
    page.get_by_role("link", name="Deploys").first.click()
    expect(page.locator("tbody tr").first.locator("[data-status]")).to_have_attribute(
        "data-status", "succeeded"
    )


def main() -> None:
    command, *args = sys.argv[1:]
    steps = {"enroll": enroll, "configure": configure, "deploy": deploy, "verify": verify}
    with sync_playwright() as p:
        browser = p.chromium.launch()
        page = browser.new_page(viewport={"width": 1280, "height": 900})
        page.set_default_timeout(30_000)
        try:
            login(page)
            steps[command](page, *args)
        except BaseException:
            os.makedirs("/tmp/e2e", exist_ok=True)
            page.screenshot(path=f"/tmp/e2e/{command}.png", full_page=True)
            raise
        finally:
            browser.close()


if __name__ == "__main__":
    main()
