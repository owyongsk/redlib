#!/usr/bin/env python3
"""Smoke test a deployed Redlib instance.

    python3 scripts/smoke_test.py [base_url]

The base URL defaults to https://rddt.fly.dev. Requests are sent one at a
time with a pause between them, so a run doesn't get the instance blocked.
Exits with status 1 if any check fails.
"""

import html
import re
import sys
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from email.utils import parsedate_to_datetime
from datetime import datetime, timezone

BASE = (sys.argv[1] if len(sys.argv) > 1 else "https://rddt.fly.dev").rstrip("/")
PAUSE_SECONDS = 1
CONTENT = "{http://purl.org/rss/1.0/modules/content/}encoded"
POST_LINK = re.compile(r'href="(/r/[^/"]+/comments/[a-z0-9]+/[^"]*)"')


class CheckFailed(Exception):
    pass


def get(path):
    time.sleep(PAUSE_SECONDS)
    request = urllib.request.Request(BASE + path, headers={"User-Agent": "redlib-smoke-test"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def page(path):
    status, body = get(path)
    if status != 200:
        error = re.search(r'<div id="error">(.*?)</div>', body, re.S)
        detail = re.sub(r"<[^>]+>|\s+", " ", error.group(1)).strip() if error else body[:200]
        raise CheckFailed(f"HTTP {status}: {html.unescape(detail)[:300]}")
    return body


def expect(condition, message):
    if not condition:
        raise CheckFailed(message)


def listing(path, minimum=5):
    body = page(path)
    count = body.count('class="post_title"')
    expect(count >= minimum, f"{count} posts, expected at least {minimum}")
    return body


def feed(path):
    root = ET.fromstring(page(path))
    items = root.findall("./channel/item")
    expect(items, "feed has no items")
    return items


def check_new_feed():
    items = feed("/r/AskReddit.rss?sort=new&limit=100")
    expect(len(items) >= 50, f"{len(items)} items, expected at least 50")
    newest = max(parsedate_to_datetime(i.findtext("pubDate")) for i in items)
    age = (datetime.now(timezone.utc) - newest).total_seconds() / 60
    expect(age < 60, f"newest post is {age:.0f} minutes old, so the feed isn't sorted by new")


def check_feed_bodies():
    items = feed("/r/Charity.rss?sort=new")
    with_body = sum(1 for i in items if (i.findtext(CONTENT) or "").strip())
    expect(with_body >= len(items) // 2, f"only {with_body} of {len(items)} items have a body")


state = {}


def check_post_with_comments():
    links = POST_LINK.findall(listing("/r/AskReddit/top?t=day"))
    expect(links, "no post links on r/AskReddit/top")
    body = page(links[0])
    expect('class="post_title"' in body, "post page has no title")
    count = body.count('class="comment_body')
    expect(count >= 3, f"{count} comments, expected at least 3")
    author = re.search(r'class="comment_author[^"]*" href="/u(?:ser)?/([A-Za-z0-9_-]+)"', body)
    state["user"] = author.group(1) if author else None
    images = re.findall(r'src="(/(?:img|preview|thumb)/[^"]+)"', body)
    state["image"] = html.unescape(images[0]) if images else None


def check_user():
    expect(state.get("user"), "no comment author found on the post page")
    body = page(f"/user/{state['user']}")
    expect('id="user_name"' in body, "user page has no profile header")


def check_media_proxy():
    if not state.get("image"):
        body = listing("/r/pics")
        images = re.findall(r'src="(/(?:img|preview|thumb)/[^"]+)"', body)
        expect(images, "no proxied images on r/pics")
        state["image"] = html.unescape(images[0])
    status, _ = get(state["image"])
    expect(status == 200, f"HTTP {status} for {state['image']}")


def check_search():
    body = page("/r/Charity/search?q=donate&restrict_sr=on")
    expect('class="post_title"' in body, "no search results")


CHECKS = [
    ("settings page", lambda: page("/settings")),
    ("instance info", lambda: page("/info")),
    ("front page", lambda: listing("/")),
    ("subreddit hot", lambda: listing("/r/Charity")),
    ("subreddit new", lambda: listing("/r/AskReddit/new")),
    ("subreddit rising", lambda: listing("/r/AskReddit/rising")),
    ("subreddit top of week", lambda: listing("/r/Charity/top?t=week")),
    ("RSS sorted by new", check_new_feed),
    ("RSS post bodies", check_feed_bodies),
    ("post with comments", check_post_with_comments),
    ("user profile", check_user),
    ("media proxy", check_media_proxy),
    ("search", check_search),
]


def main():
    print(f"Smoke testing {BASE}")
    failures = 0
    for name, check in CHECKS:
        started = time.monotonic()
        try:
            check()
            result = "ok"
        except Exception as e:
            failures += 1
            result = f"FAIL {e}"
        print(f"{name:<24} {time.monotonic() - started:5.1f}s  {result}")
    print(f"\n{len(CHECKS) - failures} passed, {failures} failed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
