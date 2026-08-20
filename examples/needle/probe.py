#!/usr/bin/env python3
"""Timed needle queries against the flooded store.

Each beat is one invocation: `probe.py --port P <beat>`. Counts are asserted
hard — a wrong count exits non-zero. Latencies are printed as measured, with
only generous sanity bounds (lookup p95 < 250 ms, everything else seconds-wide)
so CI catches pathology without flaking on machine variance.

Where the endpoint returns the engine's own cost object
({elapsed_ns, segments_examined, segments_pruned}), it is printed next to the
client wall time, because the two measure different things: the wall time
includes HTTP and JSON, the cost object is time inside the engine.
"""

import argparse
import http.client
import json
import sys
import time
import urllib.parse

DAY_NS = 86_400_000_000_000
NEEDLE_TRACE = "needle-trace-1"


class Client:
    def __init__(self, port):
        self.conn = http.client.HTTPConnection("127.0.0.1", port)

    def get(self, path):
        t0 = time.perf_counter_ns()
        self.conn.request("GET", path)
        resp = self.conn.getresponse()
        body = resp.read()
        ms = (time.perf_counter_ns() - t0) / 1e6
        return resp.status, json.loads(body), ms


def fail(msg):
    print("  FAIL: " + msg)
    sys.exit(1)


def fmt_cost(cost):
    return "cost {elapsed_ns: %s, segments_examined: %d, segments_pruned: %d}" % (
        "{:,}".format(cost["elapsed_ns"]),
        cost["segments_examined"],
        cost["segments_pruned"],
    )


def beat_lookup(c, args):
    status, body, _ = c.get("/v1/traces/" + NEEDLE_TRACE)
    if status != 200:
        fail("trace lookup answered HTTP %d" % status)
    spans = body["spans"]
    if len(spans) != 1 or spans[0]["service"] != "haystack":
        fail("expected exactly 1 haystack span in %s, got %d" % (NEEDLE_TRACE, len(spans)))
    times = []
    for _ in range(200):
        status, _, ms = c.get("/v1/traces/" + NEEDLE_TRACE)
        if status != 200:
            fail("trace lookup answered HTTP %d mid-run" % status)
        times.append(ms)
    times.sort()
    p50, p95 = times[99], times[189]
    print(
        "  200 lookups of %s: p50 %.2f ms, p95 %.2f ms  (client wall, keep-alive)"
        % (NEEDLE_TRACE, p50, p95)
    )
    if p95 >= 250:
        fail("lookup p95 %.2f ms breaches the 250 ms sanity bound" % p95)


def beat_attr(c, args):
    status, body, ms = c.get("/v1/spans?attr.needle=true&limit=10")
    if status != 200:
        fail("attribute filter answered HTTP %d" % status)
    spans = body["spans"]
    if len(spans) != 1:
        fail("attr.needle=true matched %d spans, expected exactly 1" % len(spans))
    if spans[0]["trace_id"] != NEEDLE_TRACE:
        fail("attr.needle=true found the wrong span: %s" % spans[0]["trace_id"])
    print("  exactly 1 hit — client %.2f ms; %s" % (ms, fmt_cost(body["cost"])))
    if ms >= 5000:
        fail("attribute filter took %.0f ms, past the 5 s sanity bound" % ms)


def beat_content(c, args):
    q = urllib.parse.urlencode({"q": "aubergine midnight", "limit": 10})
    status, body, ms = c.get("/v1/spans?" + q)
    if status != 200:
        fail("content search answered HTTP %d" % status)
    spans = body["spans"]
    if len(spans) != 1:
        fail("q=aubergine midnight matched %d spans, expected exactly 1" % len(spans))
    if spans[0]["trace_id"] != NEEDLE_TRACE:
        fail("content search found the wrong span: %s" % spans[0]["trace_id"])
    if "aubergine" not in spans[0]["attributes"].get("note", ""):
        fail("the hit does not carry the sentence")
    cost = body["cost"]
    print(
        "  exactly 1 hit — engine %.2f ms, client %.2f ms; %s"
        % (cost["elapsed_ns"] / 1e6, ms, fmt_cost(cost))
    )
    if ms >= 5000:
        fail("content search took %.0f ms, past the 5 s sanity bound" % ms)


def beat_absent(c, args):
    status, body, ms = c.get("/v1/spans?q=xylotheque&limit=10")
    if status != 200:
        fail("absent-word search answered HTTP %d" % status)
    if len(body["spans"]) != 0:
        fail("q=xylotheque matched %d spans, expected 0" % len(body["spans"]))
    cost = body["cost"]
    print(
        "  0 hits — engine %.0f µs, client %.2f ms; %d of %d segments pruned"
        " without being read"
        % (
            cost["elapsed_ns"] / 1e3,
            ms,
            cost["segments_pruned"],
            cost["segments_examined"],
        )
    )
    if cost["elapsed_ns"] >= 250_000_000:
        fail("absent-word engine time %d ns breaches the 250 ms sanity bound"
             % cost["elapsed_ns"])


def beat_window(c, args):
    since = time.time_ns() - 2 * DAY_NS
    status, body, ms = c.get("/v1/spans?service=checkout&since=%d&limit=50" % since)
    if status != 200:
        fail("time-window query answered HTTP %d" % status)
    spans = body["spans"]
    if len(spans) < 1:
        fail("no checkout spans in the last 2 days; the flood spread should place some")
    cost = body["cost"]
    print(
        "  %d of %d segments pruned by the time range — %d spans on the first page,"
        " engine %.2f ms, client %.2f ms"
        % (
            cost["segments_pruned"],
            cost["segments_examined"],
            len(spans),
            cost["elapsed_ns"] / 1e6,
            ms,
        )
    )
    if ms >= 5000:
        fail("time-window query took %.0f ms, past the 5 s sanity bound" % ms)


def beat_aggregate(c, args):
    status, body, ms = c.get("/v1/stats/duration")
    if status != 200:
        fail("duration aggregate answered HTTP %d" % status)
    if body["count"] != args.expect_spans:
        fail("aggregate folded %d spans, %d were acknowledged"
             % (body["count"], args.expect_spans))
    print(
        "  %s spans folded in %.1f ms client wall — duration p50 %.0f ms, p95 %.0f ms"
        % ("{:,}".format(body["count"]), ms, body["p50_ns"] / 1e6, body["p95_ns"] / 1e6)
    )
    if ms >= 30000:
        fail("whole-corpus aggregate took %.0f ms, past the 30 s sanity bound" % ms)


def beat_store(c, args):
    status, body, _ = c.get("/v1/stats")
    if status != 200:
        fail("/v1/stats answered HTTP %d" % status)
    if args.expect_spans and body["record_count"] != args.expect_spans:
        fail("/v1/stats reports %d records, %d spans were acknowledged"
             % (body["record_count"], args.expect_spans))
    mib = body["bytes_on_disk"] / (1024 * 1024)
    size = "%.2f GiB" % (mib / 1024) if mib >= 1024 else "%.0f MiB" % mib
    print(
        "  %s on disk across %d segments, %s records — asserted equal to the"
        " acknowledged count"
        % (size, body["segment_count"], "{:,}".format(body["record_count"]))
    )


BEATS = {
    "lookup": beat_lookup,
    "attr": beat_attr,
    "content": beat_content,
    "absent": beat_absent,
    "window": beat_window,
    "aggregate": beat_aggregate,
    "store": beat_store,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--expect-spans", type=int, default=0)
    ap.add_argument("beat", choices=sorted(BEATS))
    args = ap.parse_args()
    BEATS[args.beat](Client(args.port), args)


if __name__ == "__main__":
    main()
