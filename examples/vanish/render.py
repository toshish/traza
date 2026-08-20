#!/usr/bin/env python3
"""Renders and asserts one step of the vanish demo.

Kept out of the shell script deliberately: inline `python3 -c` inside single
quotes cannot use double quotes without escaping, and the escaping is where
this kind of script goes wrong. Every mode both prints what happened and
exits non-zero when what happened is not what the demo claims.
"""

import json
import sys
import threading
import time
import http.client


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def request(port, token, method, path, body=None):
    connection = http.client.HTTPConnection("127.0.0.1", int(port))
    headers = {"Authorization": "Bearer " + token}
    if body is not None:
        headers["Content-Type"] = "application/json"
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    data = response.read()
    connection.close()
    return response.status, data


def short_ref(reference):
    return reference[:14] + "…" + reference[-6:] if len(reference) > 24 else reference


# ---------------------------------------------------------------- modes


def tenants(path, blob_bytes):
    """The per-tenant ledger, and proof both tenants are really in one store."""
    blob_bytes = int(blob_bytes)
    rows = load(path)["tenants"]
    by_name = {row["tenant"]: row for row in rows}
    must("acme" in by_name and "zenith" in by_name, "expected acme and zenith rows")
    print("  {:<8} {:>7} {:>8} {:>14} {:>16}".format(
        "tenant", "spans", "traces", "bytes_approx", "payload_bytes"))
    for row in rows:
        must(row["spans"] > 0, "tenant %r holds no spans" % row["tenant"])
        print("  {:<8} {:>7} {:>8} {:>14} {:>16}".format(
            row["tenant"] or '""', row["spans"], row["traces"],
            row["bytes_approx"], row["payload_bytes_approx"]))
    for who in ("acme", "zenith"):
        must(by_name[who]["payload_bytes_approx"] >= blob_bytes,
             "%s payload_bytes_approx %d does not cover the %d-byte transcript blob"
             % (who, by_name[who]["payload_bytes_approx"], blob_bytes))
    print()
    print("  payload bytes count for every referencing tenant: the shared")
    print("  transcript blob (%d bytes) is one file on disk, held against both"
          % blob_bytes)
    print("  quotas — each tenant's payload_bytes above covers it (asserted).")


def pick_trace(path):
    """The first span's trace id — present in both tenants by construction."""
    print(load(path)["spans"][0]["trace_id"])


def trace_copies(path_acme, path_zenith):
    acme = len(load(path_acme)["spans"])
    zenith = len(load(path_zenith)["spans"])
    must(acme > 0 and zenith > 0, "both tenants should hold this trace")
    must(acme == zenith, "identical corpora should give equal span counts")
    print("  acme sees %d spans · zenith sees %d spans — same trace id," % (acme, zenith))
    print("  two tenants' copies, byte-for-byte independent rows (counts equal, asserted)")


def pick_session(path):
    """The widest-spread acme session: most traces, then most spans."""
    rows = load(path)["sessions"]
    must(rows, "no sessions in the seeded corpus")
    rows.sort(key=lambda row: (-row["trace_count"], -row["span_count"], row["session_id"]))
    chosen = rows[0]
    print(chosen["session_id"], chosen["span_count"])


def enrich(port, acme_token, zenith_token, session_id, ref_out, size_out):
    """One annotation, and one oversized transcript in BOTH tenants.

    The transcript is identical in both, so the content-addressed payload
    store holds it once — one file, two tenants' spans referencing it. That
    is what makes the settle block's payload accounting worth reading.
    """
    status, body = request(port, acme_token, "POST", "/v1/annotations", json.dumps({
        "session_id": session_id, "name": "thumbs", "value": "down",
        "source": "human:support",
    }))
    must(status == 200, "annotation refused: %d %s" % (status, body.decode()))
    print("  recorded a support annotation on the session (thumbs=down, human:support)")

    transcript = "user: please delete everything you hold about me\n" * 6000
    now = time.time_ns()
    span = {
        "trace_id": "trace-dsr-0001", "span_id": "span-dsr-0001",
        "name": "conversation.export", "service": "support", "status": "ok",
        "start_time_ns": now - 5_000_000, "end_time_ns": now,
        "attributes": {"session.id": session_id, "export.body": transcript},
    }
    for token, who in ((acme_token, "acme"), (zenith_token, "zenith")):
        status, body = request(port, token, "POST", "/v1/spans", json.dumps([span]))
        must(status == 200, "%s transcript ingest refused: %d" % (who, status))
    print("  attached the exported transcript (%d KiB) to the session in both tenants,"
          % (len(transcript) // 1024))
    print("  under the same trace id: trace-dsr-0001")

    references = {}
    for token, who in ((acme_token, "acme"), (zenith_token, "zenith")):
        status, body = request(
            port, token, "GET", "/v1/spans?session=" + session_id + "&limit=100")
        must(status == 200, "session read failed for " + who)
        for row in load_spans(body):
            value = row.get("attributes", {}).get("export.body")
            if isinstance(value, dict) and "$payload" in value:
                references[who] = value["$payload"]
    must(set(references) == {"acme", "zenith"}, "transcript was not offloaded")
    must(references["acme"] == references["zenith"],
         "identical content should share one content-addressed file")
    print("  offloaded once, content-addressed: both tenants reference")
    print("  %s — one payload file on disk (asserted identical)" % references["acme"])
    with open(ref_out, "w") as handle:
        handle.write(references["acme"])
    with open(size_out, "w") as handle:
        handle.write(str(len(transcript.encode())))


def load_spans(body):
    return json.loads(body)["spans"]


def erase_race(port, admin_token, acme_token, session_id, min_spans,
               shared_ref, stash_path, env_path):
    """POST the erasure, and replay one covered span while it is pending.

    The erasure response returns only after the purge settles, so the only
    honest moment to demonstrate the admission barrier is while the request
    is in flight. The replay loop runs concurrently and stops at the first
    suppressed acknowledgement; if the window is somehow missed the demo
    fails here rather than pretending.
    """
    min_spans = int(min_spans)
    status, body = request(
        port, acme_token, "GET", "/v1/spans?session=" + session_id + "&limit=100")
    must(status == 200, "could not read the session before erasing it")
    spans = load_spans(body)
    must(spans, "the chosen session has no spans")
    covered = next(  # prefer a span with no payload reference, for a clean replay
        (row for row in spans if '"$payload"' not in json.dumps(row)), spans[0])
    replay_body = json.dumps([covered])

    outcome = {}

    def erase():
        started = time.monotonic()
        erase_status, erase_data = request(port, admin_token, "POST", "/v1/erasures",
                                           json.dumps({"subject": {
                                               "kind": "session",
                                               "session_id": session_id,
                                               "tenant": "acme"}}))
        outcome["status"] = erase_status
        outcome["body"] = erase_data
        outcome["seconds"] = time.monotonic() - started

    worker = threading.Thread(target=erase)
    connection = http.client.HTTPConnection("127.0.0.1", int(port))
    worker.start()
    attempts = 0
    suppressed = None
    while worker.is_alive():
        connection.request("POST", "/v1/spans", body=replay_body, headers={
            "Authorization": "Bearer " + acme_token,
            "Content-Type": "application/json"})
        reply = connection.getresponse()
        payload = json.loads(reply.read())
        attempts += 1
        if payload.get("suppressed"):
            suppressed = payload
            break
    worker.join()
    connection.close()

    must(outcome.get("status") == 200,
         "erasure refused: %s %s" % (outcome.get("status"), outcome.get("body")))
    must(suppressed is not None,
         "no replay landed inside the pending window; the erasure settled in "
         "%.3fs — re-run the demo" % outcome["seconds"])
    record = json.loads(outcome["body"])
    settle = record["settle"]
    keys = record["span_keys"]
    must(all(key[0] == "acme" for key in keys),
         "a resolved span key names a tenant other than acme")
    must(settle["spans_removed"] >= min_spans,
         "spans_removed %d < the session's %d visible spans"
         % (settle["spans_removed"], min_spans))
    must(settle["annotations_removed"] >= 1, "the support annotation was not removed")
    retained = settle["payloads_retained"]
    must(any(row["reference"] == shared_ref for row in retained),
         "the shared transcript payload should be retained, not destroyed")

    print("  erased session %r for tenant acme — blocked until settled: %.3fs" % (
        session_id, outcome["seconds"]))
    print("  erasure id %d, deletion published in generation %d" % (
        record["id"], settle["generation"]))
    print("  resolved to %d span keys, every one tenant=acme (asserted)" % len(keys))
    print()
    print("  settle: spans_removed        %d   (physical versions, superseded included)"
          % settle["spans_removed"])
    print("          annotations_removed  %d" % settle["annotations_removed"])
    print("          payloads_removed     %d" % len(settle["payloads_removed"]))
    for row in retained:
        print("          payloads_retained    %s" % short_ref(row["reference"]))
        print("                               %s" % row["reason"])

    with open(stash_path, "w") as handle:
        json.dump({"replay": suppressed, "attempts": attempts,
                   "covered": {"trace_id": covered["trace_id"],
                               "span_id": covered["span_id"]}}, handle)
    with open(env_path, "w") as handle:
        handle.write("eid=%d\ncovered_trace='%s'\ncovered_span='%s'\n" % (
            record["id"], covered["trace_id"], covered["span_id"]))


def session_assert(path, expected):
    row = load(path)
    must(row["span_count"] == int(expected),
         "zenith's copy changed: %d spans, expected %s" % (row["span_count"], expected))
    print("  zenith's session: %d spans across %d traces — intact, count unchanged (asserted)"
          % (row["span_count"], row["trace_count"]))


def gone(path):
    remaining = load(path)["spans"]
    must(remaining == [], "%d acme spans survived the erasure" % len(remaining))
    print("  acme's session by search: 0 spans (asserted)")


def trace_count(path):
    print(len(load(path)["spans"]))


def replay(stash_path, before_path, after_path, metric_name):
    stash = load(stash_path)
    response = stash["replay"]
    covered = stash["covered"]
    print("  re-POST of covered span %s/%s, acme rw token, during the pending window:"
          % (covered["trace_id"], covered["span_id"]))
    print("    " + json.dumps(response, sort_keys=True, separators=(",", ":")))
    if stash["attempts"] > 1:
        print("  (attempt %d of the replay loop; earlier attempts landed before the"
              % stash["attempts"])
        print("  barrier went up and were purged with the subject)")

    def counter(path):
        for line in open(path):
            parts = line.split()
            if len(parts) == 2 and parts[0] == metric_name:
                return int(parts[1])
        return 0

    before, after = counter(before_path), counter(after_path)
    print("  %s: %d → %d" % (metric_name, before, after))
    must(response.get("suppressed", 0) >= 1, "the replay was not suppressed")
    must(after - before == response["suppressed"],
         "metric delta %d does not match the suppressed count %d"
         % (after - before, response["suppressed"]))


def pin(path):
    row = load(path)
    must(row.get("verified") is True, "the backup pin did not verify")
    print("  " + json.dumps(row, sort_keys=True, separators=(",", ":")))


def release(path):
    row = load(path)
    must(row.get("released") is True, "the pin was not released")
    print("  " + json.dumps(row, sort_keys=True, separators=(",", ":")))


def receipt(path, kind, label):
    report = load(path)
    for domain in report["domains"]:
        print("  {:<18} {:<20} {}".format(
            domain["domain"], domain["result"], domain["detail"]))
        for item in domain.get("items", []):
            print("  {:<18} {:<20}   - {}".format("", "", item))
    print()
    print("  result: %s · conclusive: %s" % (
        report["result"], str(report["conclusive"]).lower()))
    pins = next(d for d in report["domains"] if d["domain"] == "pins")
    if kind == "pinned":
        must(pins["result"] == "holds-data", "the pins domain should hold data")
        must(any(label in item for item in pins.get("items", [])),
             "the receipt does not name pin %r" % label)
        must(not (report["result"] == "erased" and report["conclusive"]),
             "the receipt claimed proof while a pin still holds the bytes")
    elif kind == "final":
        must(pins["result"] == "clear", "pins should be clear after the release")
        must(report["result"] == "erased", "expected result: erased")
        must(report["conclusive"] is True, "expected a conclusive receipt")
    else:
        must(False, "unknown receipt kind %r" % kind)


MODES = {
    "tenants": tenants,
    "pick-trace": pick_trace,
    "trace-copies": trace_copies,
    "pick-session": pick_session,
    "enrich": enrich,
    "erase-race": erase_race,
    "session-assert": session_assert,
    "gone": gone,
    "trace-count": trace_count,
    "replay": replay,
    "pin": pin,
    "release": release,
    "receipt": receipt,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](*sys.argv[2:])
