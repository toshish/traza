#!/usr/bin/env python3
"""Writes the coding-agents demo's synthetic transcripts, and the project
they worked on.

Nothing here comes from a real session. Every line is authored below, in the
shapes Claude Code 2.x and the Codex CLI write to disk (the same shapes as
the importer's fixtures in tests/fixtures/import/, documented on
src/import/mod.rs), timestamped relative to the moment the demo runs so the
sessions sit inside every default window.

  transcripts.py write ROOT        write ROOT/shop, ROOT/home/.claude, ROOT/home/.codex
                                   and print the manifest (JSON) the demo asserts against
  transcripts.py append ROOT PART  append part 1 or 2 of a new turn to the rename session,
                                   for the --watch beat; prints the wall clock it wrote at

The story, in four sessions on one small Python shop:

  coupons  (Claude Code) adds coupon codes with an Explore sub-agent and a
           code-reviewer sub-agent; a 16-minute pause lets the prompt cache
           expire; a /compact summarizes away the instruction "never touch
           migrations/"; two calls later the agent writes a migration.
  tests    (Claude Code) is asked to fix a failing test and edits the test's
           expectation instead of the code.
  rename   (Claude Code) renames a variable and stops without running the
           tests. The --watch beat appends the turn where it does run them.
  endpoint (Codex) adds an API route with apply_patch and runs the tests.
"""

import datetime
import json
import os
import sys
import time

SESSION_COUPONS = "1c0a7e5b-2f4d-4e6a-8b9c-0d1e2f3a4b5c"
SESSION_TESTS = "2d1b8f6c-3a5e-4f7b-9c0d-1e2f3a4b5c6d"
SESSION_RENAME = "3e2c9a7d-4b6f-4a8c-8d1e-2f3a4b5c6d7e"
THREAD_ENDPOINT = "4f3d0b8e-5c7a-4b9d-9e2f-3a4b5c6d7e8f"

SONNET = "claude-sonnet-4-5-20250929"
HAIKU = "claude-haiku-4-5-20251001"
CODEX_MODEL = "gpt-5-codex"
CLAUDE_VERSION = "2.1.3"
CODEX_VERSION = "0.150.0"

MINUTE = 60.0

# The instruction a compaction will summarize away, and the prompt that
# leads the agent to break it afterwards.
RULE = "Never touch anything under migrations/ — those files run against production."
FIRST_PROMPT = "Add percentage coupon codes to checkout. " + RULE

# ------------------------------------------------------------ the project
#
# The files as they stand after every session: what the agents' edits left.
# The demo blames lines of these files, so each blamed line below must be a
# line some recorded edit added.

CHECKOUT_BEFORE = '''"""Order totals for the shop's checkout."""

from shop.models import LineItem


def line_total(item: LineItem) -> int:
    """One line's price in cents: unit price times quantity."""
    return item.unit_cents * item.quantity


def order_total(items: list[LineItem]) -> int:
    """The order's subtotal in cents, before shipping and tax."""
    total_cents = 0
    for item in items:
        total_cents += line_total(item)
    return total_cents


def shipping_cents(total_cents: int) -> int:
    """Free shipping over $50; a flat $4.99 below it."""
    return 0 if total_cents >= 5000 else 499
'''

CHECKOUT_COUPON_OLD = '''def order_total(items: list[LineItem]) -> int:
    """The order's subtotal in cents, before shipping and tax."""
    total_cents = 0
    for item in items:
        total_cents += line_total(item)
    return total_cents'''

CHECKOUT_COUPON_NEW = '''def order_total(items: list[LineItem], coupon: str | None = None) -> int:
    """The order's subtotal in cents, before shipping and tax."""
    total_cents = 0
    for item in items:
        total_cents += line_total(item)
    if coupon:
        total_cents = apply_coupon(total_cents, coupon)
    return total_cents'''

DISCOUNTS_V1 = '''"""Coupon codes: a percentage off the order subtotal."""

COUPONS = {"SPRING10": 10, "WELCOME15": 15, "HALF60": 60}


def apply_coupon(total_cents: int, code: str) -> int:
    """The total after the coupon's percentage."""
    percent = COUPONS.get(code, 0)
    return total_cents - total_cents * percent // 100
'''

CAP_OLD = '''    percent = COUPONS.get(code, 0)'''
CAP_NEW = '''    percent = min(COUPONS.get(code, 0), MAX_PERCENT)'''

MIGRATION = '''-- Record which coupon an order used.
ALTER TABLE orders ADD COLUMN coupon_code TEXT;
CREATE INDEX orders_coupon_code ON orders (coupon_code);
'''

TESTS_BEFORE = '''from shop.discounts import apply_coupon


def test_unknown_code_is_full_price():
    assert apply_coupon(10000, "NOPE") == 10000


def test_spring10():
    assert apply_coupon(10000, "SPRING10") == 9000


def test_codes_are_case_insensitive():
    assert apply_coupon(10000, "spring10") == 9000


def test_cap_at_half():
    assert apply_coupon(10000, "HALF60") == 5000
'''

TAMPER_OLD = '''    assert apply_coupon(10000, "spring10") == 9000'''
TAMPER_NEW = '''    assert apply_coupon(10000, "spring10") == 10000'''

API_BEFORE = '''"""The shop's HTTP routes."""

from shop.discounts import COUPONS
from shop.web import Router

router = Router()
'''

API_PATCH_LINES = [
    '',
    '',
    '@router.get("/coupons/{code}")',
    'def coupon(code: str) -> dict:',
    '    return {"code": code, "percent": COUPONS.get(code, 0)}',
]

MODELS = '''"""The shop's data model."""

from dataclasses import dataclass


@dataclass
class LineItem:
    sku: str
    unit_cents: int
    quantity: int
'''


def final_files():
    """Every file of the shop as the sessions left it."""
    checkout = CHECKOUT_BEFORE.replace(CHECKOUT_COUPON_OLD, CHECKOUT_COUPON_NEW)
    checkout = checkout.replace(
        "from shop.models import LineItem\n",
        "from shop.discounts import apply_coupon\nfrom shop.models import LineItem\n")
    discounts = DISCOUNTS_V1.replace(CAP_OLD, CAP_NEW).replace(
        'COUPONS = {', 'MAX_PERCENT = 50\n\nCOUPONS = {')
    # The rename session's edit.
    checkout = checkout.replace("total_cents", "subtotal_cents")
    api = API_BEFORE + "\n".join(API_PATCH_LINES) + "\n"
    return {
        "src/shop/__init__.py": "",
        "src/shop/models.py": MODELS,
        "src/shop/checkout.py": checkout,
        "src/shop/discounts.py": discounts,
        "src/shop/api.py": api,
        "tests/test_discounts.py": TESTS_BEFORE.replace(TAMPER_OLD, TAMPER_NEW),
        "migrations/0005_orders.sql": "CREATE TABLE orders (id INTEGER PRIMARY KEY, total_cents INTEGER);\n",
        "migrations/0006_refunds.sql": "CREATE TABLE refunds (id INTEGER PRIMARY KEY, order_id INTEGER);\n",
        "migrations/0007_order_coupon.sql": MIGRATION,
    }


# ------------------------------------------------------------- Claude Code


def stamp(seconds):
    """RFC 3339 with milliseconds and a Z, as both tools write it."""
    moment = datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + "%03dZ" % (moment.microsecond // 1000)


def numbered(text):
    """A file as Claude Code's Read tool returns it: cat -n."""
    return "\n".join("%6d\t%s" % (n, line) for n, line in enumerate(text.splitlines(), 1))


class Transcript:
    """One Claude Code transcript file: records chained by parentUuid."""

    def __init__(self, session, cwd, clock, agent_id=None):
        self.session = session
        self.cwd = cwd
        self.clock = clock
        self.agent_id = agent_id
        self.lines = []
        self.parent = None
        self.serial = 0
        self.messages = 0

    def uuid(self):
        self.serial += 1
        prefix = self.agent_id or self.session[:8]
        return "%s-%04d" % (prefix, self.serial)

    def record(self, kind, advance, body):
        self.clock += advance
        uuid = self.uuid()
        record = {
            "parentUuid": self.parent, "isSidechain": self.agent_id is not None,
            "userType": "external", "cwd": self.cwd, "sessionId": self.session,
            "version": CLAUDE_VERSION, "gitBranch": "main", "type": kind,
            "uuid": uuid, "timestamp": stamp(self.clock),
        }
        if self.agent_id:
            record["agentId"] = self.agent_id
        record.update(body)
        self.lines.append(record)
        self.parent = uuid
        return record

    def prompt(self, text, advance=1.0):
        self.record("user", advance, {"message": {"role": "user", "content": text}})

    def respond(self, model, blocks, usage, advance=3.0):
        """One API response, written the way Claude Code writes it: one
        record per content block, all sharing the message id and usage."""
        self.messages += 1
        message_id = "msg_%s%02d" % ((self.agent_id or self.session[:6]), self.messages)
        stop = "tool_use" if any(b["type"] == "tool_use" for b in blocks) else "end_turn"
        for n, block in enumerate(blocks):
            self.record("assistant", advance if n == 0 else 0.4, {
                "requestId": "req_" + message_id,
                "message": {
                    "id": message_id, "type": "message", "role": "assistant", "model": model,
                    "content": [block], "stop_reason": stop, "stop_sequence": None,
                    "usage": usage,
                },
            })
        return message_id

    def result(self, tool_use_id, content, error=False, advance=1.0, extra=None):
        block = {"tool_use_id": tool_use_id, "type": "tool_result", "content": content}
        if error:
            block["is_error"] = True
        body = {"message": {"role": "user", "content": [block]}}
        body.update(extra or {})
        self.record("user", advance, body)

    def system(self, subtype, advance, body):
        self.clock += advance
        uuid = self.uuid()
        record = {"type": "system", "subtype": subtype, "uuid": uuid, "parentUuid": self.parent,
                  "sessionId": self.session, "timestamp": stamp(self.clock), "cwd": self.cwd,
                  "isSidechain": self.agent_id is not None, "version": CLAUDE_VERSION}
        record.update(body)
        self.lines.append(record)
        self.parent = uuid

    def compact(self, summary, advance):
        self.system("compact_boundary", advance, {
            "content": "Conversation compacted", "level": "info",
            "compactMetadata": {"trigger": "manual", "preTokens": 23840}})
        self.record("user", 0.2, {"isCompactSummary": True,
                                  "message": {"role": "user", "content": summary}})

    def text(self):
        return "".join(json.dumps(line, ensure_ascii=False) + "\n" for line in self.lines)


def usage(input_tokens, output_tokens, cache_read=0, cache_write=0):
    return {"input_tokens": input_tokens, "output_tokens": output_tokens,
            "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": cache_write}


def tool_use(tool_id, name, arguments):
    return {"type": "tool_use", "id": tool_id, "name": name, "input": arguments}


def say(text):
    return {"type": "text", "text": text}


def think(text):
    return {"type": "thinking", "thinking": text, "signature": "sig-synthetic"}


def in_project(cwd):
    """A path inside the project, absolute, as the transcripts record it."""
    return lambda rel: cwd + "/" + rel


def coupons_session(cwd, start):
    """The coupons session and its two sub-agents."""
    main = Transcript(SESSION_COUPONS, cwd, start)
    path = in_project(cwd)
    subagents = []

    # Turn 1: coupon codes, with an Explore agent and a reviewer.
    main.prompt(FIRST_PROMPT, advance=0.0)
    main.respond(SONNET, [
        think("Map the checkout code before changing it; an Explore agent is cheaper."),
        say("I'll have an Explore agent map how checkout totals are computed first."),
        tool_use("toolu_c01", "Agent", {
            "description": "Map checkout totals",
            "prompt": "Find where checkout totals are computed and where a percentage "
                      "discount could apply. Report file paths and function names.",
            "subagent_type": "Explore"}),
    ], usage(3, 214, 0, 14210))

    explore = Transcript(SESSION_COUPONS, cwd, main.clock, agent_id="a7e1c90")
    explore.prompt("Find where checkout totals are computed and where a percentage discount "
                   "could apply. Report file paths and function names.", advance=0.5)
    explore.respond(HAIKU, [tool_use("toolu_e01", "Grep", {"pattern": "total", "path": path("src")})],
                    usage(4, 61, 0, 9820), advance=2.0)
    explore.result("toolu_e01", "src/shop/checkout.py:6:def line_total(item: LineItem) -> int:\n"
                   "src/shop/checkout.py:11:def order_total(items: list[LineItem]) -> int:\n"
                   "src/shop/checkout.py:19:def shipping_cents(total_cents: int) -> int:")
    explore.respond(HAIKU, [tool_use("toolu_e02", "Read", {"file_path": path("src/shop/checkout.py")})],
                    usage(6, 48, 9820, 310), advance=1.5)
    explore.result("toolu_e02", numbered(CHECKOUT_BEFORE))
    explore.respond(HAIKU, [tool_use("toolu_e03", "Read", {"file_path": path("src/shop/models.py")})],
                    usage(5, 44, 10130, 620), advance=1.5)
    explore.result("toolu_e03", numbered(MODELS))
    report = ("Checkout totals live in src/shop/checkout.py: order_total(items) sums "
              "line_total(item) over the line items and returns the subtotal in cents; "
              "shipping_cents(total_cents) applies after it. There is no discount code yet. "
              "A percentage coupon fits as an optional argument to order_total, applied to "
              "the subtotal before shipping. LineItem is in src/shop/models.py.")
    explore.respond(HAIKU, [say(report)], usage(8, 162, 10750, 0), advance=2.5)
    subagents.append(("a7e1c90", "Explore", "Map checkout totals", "toolu_c01", explore))
    main.clock = explore.clock
    main.result("toolu_c01", [{"type": "text", "text": report}], advance=0.6,
                extra={"toolUseResult": {"agentId": "a7e1c90", "status": "completed"}})

    main.respond(SONNET, [
        say("I'll add the coupon table and the function that applies it."),
        tool_use("toolu_c02", "Write", {"file_path": path("src/shop/discounts.py"),
                                         "content": DISCOUNTS_V1}),
    ], usage(4, 286, 14210, 1240))
    main.result("toolu_c02", "File created successfully at: " + path("src/shop/discounts.py"))
    main.respond(SONNET, [tool_use("toolu_c03", "Edit", {
        "file_path": path("src/shop/checkout.py"), "old_string": CHECKOUT_COUPON_OLD,
        "new_string": CHECKOUT_COUPON_NEW, "replace_all": False})],
        usage(5, 241, 15450, 690))
    main.result("toolu_c03", "The file %s has been updated." % path("src/shop/checkout.py"))
    main.respond(SONNET, [tool_use("toolu_c04", "Bash", {"command": "pytest -q",
                                                         "description": "Run the test suite"})],
                 usage(4, 58, 16140, 380))
    main.result("toolu_c04", "....                                                [100%]\n4 passed in 0.04s",
                advance=2.4)
    main.respond(SONNET, [tool_use("toolu_c05", "Agent", {
        "description": "Review the coupon change",
        "prompt": "Review the coupon-code change in src/shop/discounts.py and "
                  "src/shop/checkout.py for bugs. Be brief.",
        "subagent_type": "code-reviewer"})], usage(4, 96, 16520, 210))

    review = Transcript(SESSION_COUPONS, cwd, main.clock, agent_id="r3v41e2")
    review.prompt("Review the coupon-code change in src/shop/discounts.py and "
                  "src/shop/checkout.py for bugs. Be brief.", advance=0.5)
    review.respond(SONNET, [tool_use("toolu_r01", "Read", {"file_path": path("src/shop/discounts.py")})],
                   usage(4, 52, 0, 8930), advance=2.0)
    review.result("toolu_r01", numbered(DISCOUNTS_V1))
    verdict = ("One real issue: nothing caps the percentage. HALF60 already takes 60% off, and "
               "a code above 100 would make the total negative. Cap the percentage (50 is "
               "typical) and test the cap. The rest reads correctly.")
    review.respond(SONNET, [say(verdict)], usage(6, 118, 8930, 420), advance=3.5)
    subagents.append(("r3v41e2", "code-reviewer", "Review the coupon change", "toolu_c05", review))
    main.clock = review.clock
    main.result("toolu_c05", [{"type": "text", "text": verdict}], advance=0.6,
                extra={"toolUseResult": {"agentId": "r3v41e2", "status": "completed"}})
    main.respond(SONNET, [say("Coupon codes are in: SPRING10, WELCOME15 and HALF60 apply to the "
                              "subtotal, and the suite passes. The reviewer flagged that "
                              "nothing caps the percentage — want a cap at 50%?")],
                 usage(4, 74, 16730, 260))

    # Turn 2, after a 16-minute pause: the cache entry has expired, and the
    # provider writes the whole prefix again (cache_read 0).
    main.prompt("Yes — cap coupons at 50% off.", advance=16 * MINUTE)
    main.respond(SONNET, [tool_use("toolu_c06", "Edit", {
        "file_path": path("src/shop/discounts.py"),
        "old_string": 'COUPONS = {"SPRING10"', "new_string": 'MAX_PERCENT = 50\n\nCOUPONS = {"SPRING10"',
        "replace_all": False})], usage(5, 112, 0, 17320))
    main.result("toolu_c06", "The file %s has been updated." % path("src/shop/discounts.py"))
    main.respond(SONNET, [tool_use("toolu_c07", "Edit", {
        "file_path": path("src/shop/discounts.py"), "old_string": CAP_OLD, "new_string": CAP_NEW,
        "replace_all": False})], usage(4, 98, 17320, 240))
    main.result("toolu_c07", "The file %s has been updated." % path("src/shop/discounts.py"))
    main.respond(SONNET, [tool_use("toolu_c08", "Bash", {"command": "pytest -q",
                                                         "description": "Run the test suite"})],
                 usage(4, 41, 17560, 190))
    main.result("toolu_c08", "....                                                [100%]\n4 passed in 0.04s",
                advance=2.2)
    main.respond(SONNET, [say("Capped at 50%: HALF60 now takes half off, and the suite passes.")],
                 usage(4, 39, 17750, 130))

    # A /compact: the summary keeps the work and drops the rule.
    main.compact("This session is being continued from a previous conversation. Summary:\n"
                 "1. Coupon codes: src/shop/discounts.py holds COUPONS (SPRING10, WELCOME15, "
                 "HALF60), MAX_PERCENT = 50 and apply_coupon(total_cents, code); "
                 "order_total(items, coupon=None) in src/shop/checkout.py applies it.\n"
                 "2. A code-reviewer sub-agent asked for a cap; the cap is 50%.\n"
                 "3. pytest -q passes (4 tests).", advance=6 * MINUTE)

    # Turn 3: two calls later, the agent writes under migrations/.
    main.prompt("Now record which coupon each order used.", advance=40.0)
    main.respond(SONNET, [
        say("I'll check the existing migrations and add a column."),
        tool_use("toolu_c09", "Glob", {"pattern": "migrations/*.sql"}),
    ], usage(4, 77, 0, 4120))
    main.result("toolu_c09", path("migrations/0005_orders.sql") + "\n" + path("migrations/0006_refunds.sql"))
    main.respond(SONNET, [tool_use("toolu_c10", "Write", {
        "file_path": path("migrations/0007_order_coupon.sql"), "content": MIGRATION})],
        usage(4, 133, 4120, 310))
    main.result("toolu_c10", "File created successfully at: " + path("migrations/0007_order_coupon.sql"))
    main.respond(SONNET, [say("Added migrations/0007_order_coupon.sql: orders.coupon_code, indexed.")],
                 usage(4, 41, 4430, 160))
    return main, subagents


def tests_session(cwd, start):
    """Asked to fix a failing test, the agent edits the test's expectation."""
    main = Transcript(SESSION_TESTS, cwd, start)
    path = in_project(cwd)
    main.prompt("The discount tests fail on main. Fix them.", advance=0.0)
    main.respond(SONNET, [tool_use("toolu_t01", "Bash", {"command": "pytest -q tests/test_discounts.py",
                                                         "description": "Run the discount tests"})],
                 usage(3, 64, 0, 12960))
    main.result("toolu_t01", "..F.                                                [100%]\n"
                "FAILED tests/test_discounts.py::test_codes_are_case_insensitive - "
                "assert 10000 == 9000\n1 failed, 3 passed in 0.05s", error=True, advance=2.6)
    main.respond(SONNET, [tool_use("toolu_t02", "Read", {"file_path": path("tests/test_discounts.py")})],
                 usage(4, 47, 12960, 410))
    main.result("toolu_t02", numbered(TESTS_BEFORE))
    main.respond(SONNET, [
        think("The test expects lowercase codes to work; the simplest change is the expectation."),
        tool_use("toolu_t03", "Edit", {"file_path": path("tests/test_discounts.py"),
                                       "old_string": TAMPER_OLD, "new_string": TAMPER_NEW,
                                       "replace_all": False}),
    ], usage(4, 151, 13370, 520))
    main.result("toolu_t03", "The file %s has been updated." % path("tests/test_discounts.py"))
    main.respond(SONNET, [tool_use("toolu_t04", "Bash", {"command": "pytest -q tests/test_discounts.py",
                                                         "description": "Run the discount tests"})],
                 usage(4, 52, 13890, 230))
    main.result("toolu_t04", "....                                                [100%]\n4 passed in 0.04s",
                advance=2.3)
    main.respond(SONNET, [say("Fixed: test_codes_are_case_insensitive expected the wrong total. "
                              "All 4 discount tests pass.")], usage(4, 44, 14120, 110))
    return main


def rename_session(cwd, start):
    """Edits and stops without running the tests (the watch beat finishes it)."""
    main = Transcript(SESSION_RENAME, cwd, start)
    path = in_project(cwd)
    main.prompt("Rename total_cents to subtotal_cents in checkout.py.", advance=0.0)
    checkout = final_files()["src/shop/checkout.py"].replace("subtotal_cents", "total_cents")
    main.respond(SONNET, [tool_use("toolu_n01", "Read", {"file_path": path("src/shop/checkout.py")})],
                 usage(3, 51, 0, 12880))
    main.result("toolu_n01", numbered(checkout))
    main.respond(SONNET, [tool_use("toolu_n02", "Edit", {"file_path": path("src/shop/checkout.py"),
                                                         "old_string": "total_cents",
                                                         "new_string": "subtotal_cents",
                                                         "replace_all": True})],
                 usage(4, 88, 12880, 640))
    main.result("toolu_n02", "The file %s has been updated. All occurrences of 'total_cents' were "
                "replaced with 'subtotal_cents'." % path("src/shop/checkout.py"))
    main.respond(SONNET, [say("Renamed total_cents to subtotal_cents throughout checkout.py.")],
                 usage(4, 36, 13520, 120))
    return main


# The --watch beat's new turn of the rename session, in two parts: the
# prompt and the call that runs the tests, then the result and the answer.
WATCH_PROMPT = "Run the tests before you call it done."


def append_watch(root, part):
    """Appends part 1 or 2 of the new turn; returns the wall clock written at."""
    manifest = json.load(open(os.path.join(root, "manifest.json")))
    transcript_path = manifest["claude_code"]["files"][SESSION_RENAME]
    with open(transcript_path) as handle:
        last = json.loads(handle.read().splitlines()[-1])
    # The new records are stamped a moment in the past, never before the
    # transcript's last record, so the steps keep their order.
    previous = datetime.datetime.strptime(last["timestamp"], "%Y-%m-%dT%H:%M:%S.%fZ").replace(
        tzinfo=datetime.timezone.utc).timestamp()
    main = Transcript(SESSION_RENAME, manifest["project"], max(previous, time.time() - 1.2))
    main.parent = last["uuid"]
    main.serial = 100 * part
    main.messages = 10 * part
    if part == 1:
        main.prompt(WATCH_PROMPT, advance=0.0)
        main.respond(SONNET, [tool_use("toolu_w01", "Bash", {"command": "pytest -q",
                                                             "description": "Run the test suite"})],
                     usage(4, 39, 0, 13790), advance=0.8)
    else:
        main.result("toolu_w01", "....                                                [100%]\n"
                    "4 passed in 0.04s", advance=0.2)
        main.respond(SONNET, [say("Ran pytest -q after the rename: 4 passed.")],
                     usage(4, 22, 13790, 90), advance=0.2)
    wrote = time.time()
    with open(transcript_path, "a") as handle:
        handle.write(main.text())
        handle.flush()
        os.fsync(handle.fileno())
    return wrote


# ------------------------------------------------------------------ Codex


def codex_rollout(cwd, start):
    """A Codex CLI rollout: one turn that patches a file and runs the tests."""
    lines = []
    clock = [start]

    def item(advance, kind, payload):
        clock[0] += advance
        lines.append({"timestamp": stamp(clock[0]), "type": kind, "payload": payload})

    totals = {"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0}

    def tokens(input_tokens, cached, output):
        last = {"input_tokens": input_tokens, "cached_input_tokens": cached,
                "cache_write_input_tokens": 0, "output_tokens": output,
                "reasoning_output_tokens": 0, "total_tokens": input_tokens + output}
        for key in totals:
            totals[key] += last[key]
        total = dict(last, input_tokens=totals["input_tokens"],
                     cached_input_tokens=totals["cached_input_tokens"],
                     output_tokens=totals["output_tokens"],
                     total_tokens=totals["input_tokens"] + totals["output_tokens"])
        item(0.1, "event_msg", {"type": "token_count", "info": {
            "total_token_usage": total, "last_token_usage": last,
            "model_context_window": 272000}, "rate_limits": None})

    prompt = "Add a GET /coupons/{code} endpoint that returns the coupon's percentage."
    item(0.0, "session_meta", {"id": THREAD_ENDPOINT, "timestamp": stamp(start), "cwd": cwd,
                               "originator": "codex_cli_rs", "cli_version": CODEX_VERSION,
                               "model_provider": "openai",
                               "base_instructions": {"text": "You are a coding agent."},
                               "git": {"branch": "main", "commit_hash": "5e1f0c2"}})
    item(1.0, "event_msg", {"type": "task_started", "turn_id": "turn-1",
                            "model_context_window": 272000})
    item(0.1, "turn_context", {"turn_id": "turn-1", "cwd": cwd, "model": CODEX_MODEL,
                               "approval_policy": "on-request", "effort": "medium"})
    item(0.1, "response_item", {"type": "message", "role": "user", "content": [
        {"type": "input_text", "text": "<environment_context>\n  <cwd>%s</cwd>\n</environment_context>" % cwd}]})
    item(0.1, "response_item", {"type": "message", "role": "user", "content": [
        {"type": "input_text", "text": prompt}]})
    item(2.0, "response_item", {"type": "reasoning", "summary": [
        {"type": "summary_text", "text": "Find the router, then add the route."}],
        "encrypted_content": "synthetic"})
    item(0.4, "response_item", {"type": "function_call", "name": "exec_command",
                                "arguments": json.dumps({"cmd": "rg -n router src/shop", "workdir": cwd}),
                                "call_id": "call_x01"})
    tokens(9120, 7680, 96)
    item(0.6, "response_item", {"type": "function_call_output", "call_id": "call_x01",
                                "output": "Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code 0\n"
                                          "Output:\nsrc/shop/api.py:6:router = Router()"})
    patch = ("*** Begin Patch\n*** Update File: src/shop/api.py\n@@\n router = Router()\n"
             + "".join("+%s\n" % line for line in API_PATCH_LINES) + "*** End Patch\n")
    item(2.4, "response_item", {"type": "custom_tool_call", "status": "completed", "call_id": "call_x02",
                                "name": "apply_patch", "input": patch})
    tokens(9480, 9088, 172)
    item(0.3, "response_item", {"type": "custom_tool_call_output", "call_id": "call_x02",
                                "output": json.dumps({"output": "Success. Updated the following files:\n"
                                                                "M src/shop/api.py\n",
                                                      "metadata": {"exit_code": 0, "duration_seconds": 0.0}})})
    item(1.8, "response_item", {"type": "function_call", "name": "exec_command",
                                "arguments": json.dumps({"cmd": "pytest -q", "workdir": cwd}),
                                "call_id": "call_x03"})
    tokens(9790, 9472, 41)
    item(2.9, "response_item", {"type": "function_call_output", "call_id": "call_x03",
                                "output": "Chunk ID: 2\nWall time: 2.7 seconds\nProcess exited with code 0\n"
                                          "Output:\n.....                                               "
                                          "[100%]\n5 passed in 0.05s"})
    item(1.5, "response_item", {"type": "message", "role": "assistant", "phase": "final_answer",
                                "content": [{"type": "output_text",
                                             "text": "Added GET /coupons/{code} to src/shop/api.py; "
                                                     "pytest -q passes (5 tests)."}]})
    tokens(9910, 9728, 38)
    item(0.1, "event_msg", {"type": "task_complete", "turn_id": "turn-1",
                            "last_agent_message": "Added GET /coupons/{code}."})
    return "".join(json.dumps(line) + "\n" for line in lines), stamp(start)


# ---------------------------------------------------------------- writing


def mangle(path):
    """Claude Code's project directory name: the cwd with separators dashed
    (kept to its last 200 characters, inside any file system's name limit)."""
    return "".join(c if c.isalnum() else "-" for c in path)[-200:]


def write(root):
    root = os.path.realpath(root)
    # The real path, as an agent's cwd would be: on macOS a temporary
    # directory under /var is /private/var, and blame canonicalizes.
    project = os.path.join(root, "shop")
    for rel, content in final_files().items():
        target = os.path.join(project, rel)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w") as handle:
            handle.write(content)

    now = time.time()
    projects = os.path.join(root, "home", ".claude", "projects", mangle(project))
    os.makedirs(projects, exist_ok=True)
    files = {}
    coupons, subagents = coupons_session(project, now - 100 * MINUTE)
    sessions = [coupons, tests_session(project, now - 60 * MINUTE),
                rename_session(project, now - 40 * MINUTE)]
    for transcript in sessions:
        target = os.path.join(projects, transcript.session + ".jsonl")
        with open(target, "w") as handle:
            handle.write(transcript.text())
        files[transcript.session] = target
    agent_dir = os.path.join(projects, SESSION_COUPONS, "subagents")
    os.makedirs(agent_dir, exist_ok=True)
    for agent_id, agent_type, description, tool_use_id, transcript in subagents:
        with open(os.path.join(agent_dir, "agent-%s.jsonl" % agent_id), "w") as handle:
            handle.write(transcript.text())
        with open(os.path.join(agent_dir, "agent-%s.meta.json" % agent_id), "w") as handle:
            json.dump({"agentType": agent_type, "description": description,
                       "toolUseId": tool_use_id, "spawnDepth": 1}, handle)

    rollout, started = codex_rollout(project, now - 50 * MINUTE)
    day = started[:10].split("-")
    sessions_dir = os.path.join(root, "home", ".codex", "sessions", *day)
    os.makedirs(sessions_dir, exist_ok=True)
    rollout_path = os.path.join(sessions_dir, "rollout-%s-%s.jsonl"
                                % (started[:19].replace(":", "-"), THREAD_ENDPOINT))
    with open(rollout_path, "w") as handle:
        handle.write(rollout)

    final = final_files()

    def line_of(rel, text):
        lines = final[rel].splitlines()
        return lines.index(text) + 1

    manifest = {
        "project": project,
        "claude_code": {"path": os.path.join(root, "home", ".claude", "projects"), "files": files},
        "codex": {"path": os.path.join(root, "home", ".codex", "sessions"), "files": [rollout_path]},
        "sessions": {"coupons": SESSION_COUPONS, "tests": SESSION_TESTS,
                     "rename": SESSION_RENAME, "endpoint": THREAD_ENDPOINT},
        "rule": RULE,
        "watch_prompt": WATCH_PROMPT,
        # The lines the demo blames: who wrote each, and why it matters.
        "blame": [
            {"file": "tests/test_discounts.py", "line": line_of("tests/test_discounts.py", TAMPER_NEW),
             "session": SESSION_TESTS, "tool": "Edit"},
            {"file": "migrations/0007_order_coupon.sql",
             "line": line_of("migrations/0007_order_coupon.sql",
                             "ALTER TABLE orders ADD COLUMN coupon_code TEXT;"),
             "session": SESSION_COUPONS, "tool": "Write"},
            {"file": "src/shop/api.py", "line": line_of("src/shop/api.py", API_PATCH_LINES[2]),
             "session": THREAD_ENDPOINT, "tool": "apply_patch"},
        ],
    }
    with open(os.path.join(root, "manifest.json"), "w") as handle:
        json.dump(manifest, handle, indent=1)
    return manifest


if __name__ == "__main__":
    if sys.argv[1] == "write":
        print(json.dumps(write(sys.argv[2])))
    elif sys.argv[1] == "append":
        print("%.6f" % append_watch(os.path.realpath(sys.argv[2]), int(sys.argv[3])))
    else:
        sys.exit("usage: transcripts.py write ROOT | append ROOT 1|2")
