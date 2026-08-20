#!/usr/bin/env python3
"""Floods a Traza server with lean spans over real HTTP, one needle among them.

Eight worker processes POST /v1/spans in batches, each batch's acknowledgement
checked against the batch size, so the total printed at the end is a sum of
server acknowledgements and not a client-side hope. Span JSON is pre-serialized
from a small set of shape templates — the point of the demo is the server's
front door, not Python's json module.

Timestamps are spread evenly across the last 30 days so that time-window
pruning has something real to prune. Exactly one span is the needle:
service "haystack", trace id "needle-trace-1", carrying a sentence no other
span contains, inserted mid-flood like any other batch member.
"""

import argparse
import http.client
import json
import multiprocessing
import queue as pyqueue
import sys
import time

DAY_NS = 86_400_000_000_000
WINDOW_NS = 30 * DAY_NS

SERVICES = ["checkout", "search-api", "billing", "inventory", "auth", "notifier"]
NAMES = {
    "checkout": ["cart.confirm", "charge.card", "order.create"],
    "search-api": ["query.parse", "index.lookup", "results.rank"],
    "billing": ["invoice.render", "ledger.post", "tax.compute"],
    "inventory": ["stock.check", "reservation.hold", "sku.sync"],
    "auth": ["token.verify", "session.refresh", "login.password"],
    "notifier": ["email.send", "push.dispatch", "digest.build"],
}
REGIONS = ["us-east", "eu-west", "ap-south"]

# The tiny content vocabulary some spans carry. Deliberately disjoint from
# every word of the needle phrase and from the absent-word probe, so the
# content-search assertions can be exact.
VOCAB = [
    "ledger", "quota", "rollout", "upstream", "cache", "webhook",
    "invoice", "replica", "shard", "backlog", "checksum", "manifest",
]

NEEDLE_PHRASE = "the aubergine sang at midnight in segment seven"
NEEDLE_TRACE = "needle-trace-1"


def build_shapes():
    """64 pre-serialized span middles: name, service, status, attributes."""
    shapes = []
    for k in range(64):
        service = SERVICES[k % len(SERVICES)]
        name = NAMES[service][k % len(NAMES[service])]
        status = "error" if k % 41 == 40 else "ok"
        attrs = {
            "region": REGIONS[k % len(REGIONS)],
            "http.status_code": 502 if status == "error" else 200,
        }
        if k % 5 == 0:
            attrs["note"] = "%s %s" % (
                VOCAB[k % len(VOCAB)],
                VOCAB[(k * 7 + 3) % len(VOCAB)],
            )
        shapes.append(
            '"name":"%s","service":"%s","status":"%s","attributes":%s'
            % (name, service, status, json.dumps(attrs, separators=(",", ":")))
        )
    return shapes


def span_json(i, shapes, window_start, step_ns):
    start = window_start + i * step_ns
    end = start + (1 + (i % 499)) * 1_000_000  # 1..499 ms
    return (
        '{"trace_id":"t-%08d","span_id":"s-%08d",%s,"start_time_ns":%d,"end_time_ns":%d}'
        % (i, i, shapes[i % 64], start, end)
    )


def needle_json(i, window_start, step_ns):
    start = window_start + i * step_ns
    end = start + 123_000_000
    attrs = json.dumps(
        {"needle": True, "note": NEEDLE_PHRASE, "phase": "mid-flood"},
        separators=(",", ":"),
    )
    return (
        '{"trace_id":"%s","span_id":"needle-span-1","name":"needle.sing",'
        '"service":"haystack","status":"ok","attributes":%s,'
        '"start_time_ns":%d,"end_time_ns":%d}' % (NEEDLE_TRACE, attrs, start, end)
    )


def worker(worker_id, workers, port, total, batch_size, window_start, step_ns,
           needle_index, queue):
    shapes = build_shapes()
    conn = http.client.HTTPConnection("127.0.0.1", port)
    headers = {"Content-Type": "application/json"}
    accepted = 0
    durability = "?"
    batches = (total + batch_size - 1) // batch_size
    for b in range(worker_id, batches, workers):
        lo = b * batch_size
        hi = min(lo + batch_size, total)
        rows = [span_json(i, shapes, window_start, step_ns) for i in range(lo, hi)]
        if lo <= needle_index < hi:
            rows[needle_index - lo] = needle_json(needle_index, window_start, step_ns)
        body = ("[" + ",".join(rows) + "]").encode()
        resp = payload = None
        for attempt in (1, 2):
            try:
                conn.request("POST", "/v1/spans", body, headers)
                resp = conn.getresponse()
                payload = json.loads(resp.read())
                break
            except (http.client.HTTPException, OSError, ValueError) as exc:
                if attempt == 2:
                    queue.put(("error", "batch %d: %s" % (b, exc)))
                    return
                conn.close()
                conn = http.client.HTTPConnection("127.0.0.1", port)
        if resp.status != 200 or payload.get("accepted") != hi - lo:
            queue.put(("error", "batch %d: HTTP %d %s" % (b, resp.status, payload)))
            return
        accepted += payload["accepted"]
        durability = payload.get("durability", "?")
    queue.put(("ok", accepted, durability))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--spans", type=int, default=1_000_000)
    ap.add_argument("--workers", type=int, default=8)
    ap.add_argument("--batch", type=int, default=1000)
    args = ap.parse_args()

    window_end = time.time_ns()
    window_start = window_end - WINDOW_NS
    step_ns = max(1, WINDOW_NS // args.spans)
    needle_index = args.spans // 2

    ctx = multiprocessing.get_context()
    queue = ctx.Queue()
    t0 = time.perf_counter()
    procs = [
        ctx.Process(
            target=worker,
            args=(w, args.workers, args.port, args.spans, args.batch,
                  window_start, step_ns, needle_index, queue),
        )
        for w in range(args.workers)
    ]
    for p in procs:
        p.start()

    total = 0
    durability = "?"
    failures = []
    remaining = len(procs)
    # Workers report exactly once each, ok or error. A worker that dies
    # without reporting (OOM kill, SIGKILL) would leave a plain queue.get()
    # blocked forever, so the wait is a timed loop that checks exit codes.
    while remaining:
        try:
            msg = queue.get(timeout=2)
        except pyqueue.Empty:
            dead = [p for p in procs if p.exitcode not in (None, 0)]
            if dead:
                for p in procs:
                    if p.is_alive():
                        p.terminate()
                print("  FAIL: a flood worker died without reporting "
                      "(exit code %s)" % dead[0].exitcode)
                sys.exit(1)
            if all(p.exitcode is not None for p in procs):
                print("  FAIL: a flood worker exited without reporting a result")
                sys.exit(1)
            continue
        remaining -= 1
        if msg[0] == "ok":
            total += msg[1]
            durability = msg[2]
        else:
            failures.append(msg[1])
    for p in procs:
        p.join()
    wall = time.perf_counter() - t0

    for f in failures:
        print("  FAIL: " + f)
    if failures:
        sys.exit(1)
    if total != args.spans:
        print("  FAIL: %d spans acknowledged, %d sent" % (total, args.spans))
        sys.exit(1)

    print(
        "  %s spans acknowledged (%s) in %.1fs — %s spans/s"
        % ("{:,}".format(total), durability, wall, "{:,.0f}".format(total / wall))
    )
    print(
        "  every batch's acknowledgement checked against its size; "
        "the needle went in at index %s" % "{:,}".format(needle_index)
    )


if __name__ == "__main__":
    main()
