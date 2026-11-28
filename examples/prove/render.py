#!/usr/bin/env python3
"""Renders and asserts one step of the prove demo.

Everything that checks the server's claims here uses the Python standard
library and nothing from Traza: the head formula, the subject digest and the
evidence-pack digest are recomputed from the documented formats
(docs/guide/ledger.md), so a match means the documents say what they claim
to, not that Traza agrees with itself.

Kept out of the shell script for the reason the other demos keep theirs:
quoting JSON inside `python3 -c` is where scripts like this go wrong. Every
mode prints what happened and exits non-zero when it is not what the demo
claims.
"""

import hashlib
import hmac
import http.client
import json
import os
import sys
import time


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


# ---------------------------------------------------------------- ingest


# What `ingest` sends, kept in one place because beat 5 checks the evidence
# pack's activity counts against it: each conversation is its own session and
# trace of SPANS_PER_SESSION spans, and one span of one of them is an error.
SPANS_PER_SESSION = 4
ERROR_AT = (2, 3)  # (conversation, span) of the one error span


def conversations(subject):
    return [(subject, "jane.doe@example.com asked to close her account"),
            ("support-2207", "a customer asked about invoice 88-1"),
            ("support-2208", "a customer reported a login loop")]


def ingest(port, token, subject):
    """Three support conversations; one of them belongs to the data subject."""
    now = time.time_ns()
    sessions = conversations(subject)
    spans = []
    for s, (session, question) in enumerate(sessions):
        trace = hashlib.sha256(session.encode()).hexdigest()[:32]
        for i in range(SPANS_PER_SESSION):
            start = now + (s * 10 + i) * 1_000_000
            spans.append({
                "trace_id": trace, "span_id": "%s-%d" % (trace[:8], i),
                "name": "chat support-model", "service": "support-agent",
                "status": "error" if (s, i) == ERROR_AT else "ok",
                "start_time_ns": start, "end_time_ns": start + 400_000,
                "attributes": {
                    "session.id": session,
                    "gen_ai.operation.name": "chat",
                    "gen_ai.request.model": "support-model",
                    "gen_ai.usage.input_tokens": 300 + 40 * i,
                    "gen_ai.usage.output_tokens": 60,
                    "llm.cost_usd": 0.0015,
                    "gen_ai.input.messages": json.dumps(
                        [{"role": "user", "parts": [{"type": "text", "content": question}]}]),
                }})
    connection = http.client.HTTPConnection("127.0.0.1", int(port), timeout=10)
    connection.request("POST", "/v1/spans", body=json.dumps(spans), headers={
        "Authorization": "Bearer " + token, "Content-Type": "application/json"})
    reply = connection.getresponse()
    raw = reply.read()
    body = json.loads(raw)
    must(reply.status == 200 and body.get("accepted") == len(spans),
         "ingest answered %d: %s" % (reply.status, body))
    print("  %d spans in %d sessions → %d %s"
          % (len(spans), len(sessions), reply.status, raw.decode()))


# ------------------------------------------------------------- the chain


def describe(entry):
    kind = entry["kind"]
    if kind == "genesis":
        return "store_id %s" % entry["store_id"]
    if kind == "checkpoint":
        return "generation %d · manifest %s…" % (entry["generation"], entry["manifest_sha256"][:16])
    if kind == "erasure":
        return "erasure %d · %s · subject_digest %s…" % (
            entry["erasure_id"], entry["subject_kind"], entry["subject_digest"][:16])
    if kind == "anchor":
        return "anchors seq %d · authority %s" % (entry["target_seq"], entry["authority"])
    return ""


def chain(path):
    doc = load(path)
    for entry in doc["entries"]:
        print("    %d  %-10s %s…  %s" % (entry["seq"], entry["kind"], entry["head"][:16], describe(entry)))


def head(path):
    doc = load(path)
    must(len(doc["head"]) == 64, "a head is 64 hex characters: %s" % doc)
    print("  seq %d · head %s" % (doc["seq"], doc["head"]))
    print("  store_id %s" % doc["store_id"])


def verify_live(path):
    doc = load(path)
    gen = doc["generation"]
    must(doc["intact"] is True and not doc["problems"], "verify should be intact: %s" % doc)
    must(gen["matches_ledger"] and gen["files_verified"], "generation check failed: %s" % gen)
    print("    intact %s · %d entries · generation %d matches_ledger %s · files_verified %s"
          % (json.dumps(doc["intact"]), doc["entries"], gen["id"],
             json.dumps(gen["matches_ledger"]), json.dumps(gen["files_verified"])))


def recompute(ledger_file, head_path, how):
    """The ten-line check from docs/guide/ledger.md, run on the file itself.

    `tip`: the file's last head must be the head the server reported.
    `holds`: the server's last reported head must still be in the chain at
    its seq — what is checkable once the server is gone, whatever it may have
    appended on the way down.
    """
    prev, ok, heads = "0" * 64, True, {}
    with open(ledger_file, encoding="utf-8") as handle:
        for n, line in enumerate(handle):
            entry = json.loads(line)
            claimed = entry.pop("head")
            want = hashlib.sha256(("traza-ledger/1\n%s\n%s" % (prev, canonical(entry))).encode()).hexdigest()
            good = entry["seq"] == n and entry["prev"] == prev and claimed == want
            ok = ok and good
            prev, heads[entry["seq"]] = claimed, claimed
    must(ok, "a recomputed head did not match")
    reported = load(head_path)
    last = max(heads)
    print("    recomputed %d heads from ledger.jsonl with hashlib: every one matches," % len(heads))
    if how == "tip":
        must((last, heads[last]) == (reported["seq"], reported["head"]),
             "the file's last head is not the head the server reports: %s" % reported)
        print("    every prev links, and the last is the server's head (asserted)")
    else:
        must(heads.get(reported["seq"]) == reported["head"],
             "the chain no longer holds the head the server reported: %s" % reported)
        print("    every prev links, and the head the server last reported (seq %d) is"
              % reported["seq"])
        print("    still in the chain (asserted)")


# ------------------------------------------------------------- anchoring


def sign(head_path, key_path, out_path):
    """The stand-in notary: an HMAC-SHA256 key that lives outside the store.

    Any scheme a notary uses works the same way — Traza stores the signature
    and never checks it. What matters is that the key, and a copy of the head
    it signed, are kept somewhere the store's owner cannot rewrite.
    """
    tip = load(head_path)
    key = os.urandom(32)
    with open(key_path, "wb") as handle:
        handle.write(key)
    signature = hmac.new(key, tip["head"].encode(), hashlib.sha256).hexdigest()
    body = {"target_seq": tip["seq"], "target_head": tip["head"],
            "authority": "demo-notary (an HMAC key outside the store)",
            "anchored_at_ns": time.time_ns(), "signature": "hmac-sha256:" + signature,
            "public_key": "key-id:" + hashlib.sha256(key).hexdigest()[:16],
            "note": "prove demo"}
    with open(out_path, "w") as handle:
        json.dump(body, handle)
    print("  the notary signed seq %d's head; the key and the signed head stay outside"
          % tip["seq"])
    print("  the data directory")


def anchored(path):
    entry = load(path)
    must(entry["kind"] == "anchor", "expected an anchor entry: %s" % entry)
    print("    201 · seq %d · kind anchor · target_seq %d · head %s…"
          % (entry["seq"], entry["target_seq"], entry["head"][:16]))


def check_signature(ledger_file, key_path, signed_head_path):
    """Read the anchor back off the disk and check it with the notary's key,
    against the notary's own copy of the head it signed."""
    with open(key_path, "rb") as handle:
        key = handle.read()
    signed = load(signed_head_path)
    with open(ledger_file, encoding="utf-8") as handle:
        entries = {e["seq"]: e for e in map(json.loads, handle)}
    anchors = [e for e in entries.values() if e["kind"] == "anchor"]
    must(anchors, "no anchor entry in the chain")
    anchor = anchors[-1]
    must(anchor["target_seq"] == signed["seq"], "the anchor names seq %d, the notary signed seq %d"
         % (anchor["target_seq"], signed["seq"]))
    must(entries[signed["seq"]]["head"] == signed["head"],
         "the chain's head at seq %d is not the head the notary signed" % signed["seq"])
    expected = "hmac-sha256:" + hmac.new(key, signed["head"].encode(), hashlib.sha256).hexdigest()
    must(hmac.compare_digest(anchor["signature"], expected), "the stored signature does not verify")
    print("    the chain's head at seq %d is still the head the notary signed, and the"
          % signed["seq"])
    print("    stored signature verifies with the notary's key (checked here; Traza")
    print("    stores signatures and never checks them)")


# --------------------------------------------------------------- erasure


def erased(path):
    doc = load(path)
    settle = doc["settle"]
    must(settle is not None and settle["spans_removed"] > 0, "the erasure removed nothing: %s" % doc)
    print("    erasure %d settled · spans_removed %d · published in generation %d"
          % (doc["id"], settle["spans_removed"], settle["generation"]))


def erasure_entry(ledger_path, ledger_file, subject_id):
    entries = load(ledger_path)["entries"]
    found = [e for e in entries if e["kind"] == "erasure"]
    must(found, "the chain has no erasure entry")
    entry = found[-1]
    print("    the erasure entry, verbatim:")
    print("      " + json.dumps(entry, separators=(",", ":")))
    # The subject as GET /v1/erasures shows it, default tenant: no tenant key.
    subject = {"kind": "session", "session_id": subject_id}
    digest = hashlib.sha256(canonical(subject).encode()).hexdigest()
    must(entry["subject_digest"] == digest,
         "subject_digest %s is not sha256 of %s (%s)" % (entry["subject_digest"], canonical(subject), digest))
    with open(ledger_file, "rb") as handle:
        raw = handle.read()
    must(subject_id.encode() not in raw, "the session id appears in ledger.jsonl")
    print()
    print("    sha256(%s)" % canonical(subject))
    print("      = %s — the entry's subject_digest (asserted)" % digest)
    print("    occurrences of %r in ledger.jsonl, byte for byte: 0 (asserted)" % subject_id)


# -------------------------------------------------------------- evidence


class Raw(str):
    """A non-integer number, kept exactly as the server wrote it."""


def canon(value):
    if isinstance(value, dict):
        return "{" + ",".join(json.dumps(k, ensure_ascii=False) + ":" + canon(value[k])
                              for k in sorted(value)) + "}"
    if isinstance(value, list):
        return "[" + ",".join(canon(item) for item in value) + "]"
    return str(value) if isinstance(value, Raw) else json.dumps(value, ensure_ascii=False)


def evidence(path, subject_id):
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    pack = json.loads(text, parse_float=Raw)
    claimed = pack.pop("digest")
    digest = hashlib.sha256(canon(pack).encode()).hexdigest()
    must(digest == claimed, "evidence digest mismatch: recomputed %s, pack says %s" % (digest, claimed))
    must(pack["ledger"]["verification"]["intact"] is True, "the pack's verification is not intact")
    activity = pack["activity"]
    print("    format %s · engine %s · ledger head seq %d · verification intact %s"
          % (pack["format"], pack["engine_version"], pack["ledger"]["head"]["seq"],
             json.dumps(pack["ledger"]["verification"]["intact"])))
    # Activity counts spans still stored, so the erased conversation must be
    # gone from every count — measured against what `ingest` sent, not
    # against what the server says it holds.
    sessions = conversations(subject_id)
    kept = [s for s, (session, _) in enumerate(sessions) if session != subject_id]
    want = {"spans": SPANS_PER_SESSION * len(kept), "traces": len(kept),
            "sessions": len(kept), "errors": sum(1 for s in kept if s == ERROR_AT[0])}
    got = {key: activity[key] for key in want}
    must(got == want, "activity should count only the %d conversations still stored: wanted %s, got %s"
         % (len(kept), want, got))
    print("    activity: spans %d · traces %d · sessions %d · errors %d — the %d conversations"
          % (got["spans"], got["traces"], got["sessions"], got["errors"], len(kept)))
    print("    still stored; the erased one's %d spans are in no count (asserted)" % SPANS_PER_SESSION)
    erasures = pack["erasures"]
    must(len(erasures) == 1, "the pack should list the one erasure: %s" % erasures)
    item = erasures[0]
    must((item["subject_kind"], item["result"], item["conclusive"]) == ("session", "erased", True),
         "the erasure's receipt should be a conclusive `erased`: %s" % item)
    print("    erasure %d: %s · result %s · conclusive %s (asserted)" % (
        item["id"], item["subject_kind"], item["result"], json.dumps(item["conclusive"])))
    # A pack is counts and digests, never ids: an auditor can be handed one
    # without being handed whose sessions they were. That is a property of
    # the format, not something the erasure did, so it is checked for every
    # session, the two that were kept included.
    named = [session for session, _ in sessions if session in text]
    must(not named, "the evidence pack names sessions: %s" % named)
    print("    none of the %d session ids occurs in the pack — it carries counts, not ids"
          % len(sessions))
    print("    (asserted)")
    print()
    print("    digest in the pack:   %s" % claimed)
    print("    recomputed (hashlib): %s — match (asserted)" % digest)
    # The control: one count changed, and the digest no longer matches.
    pack["activity"]["spans"] = activity["spans"] + 1
    tampered = hashlib.sha256(canon(pack).encode()).hexdigest()
    must(tampered != claimed, "a changed pack should not keep its digest")
    print("    the same pack with activity.spans + 1 recomputes to %s… — mismatch," % tampered[:16])
    print("    as it should be")


# ---------------------------------------------------------------- damage


def flip(data_dir):
    """Flip one bit in the middle of the largest segment the live manifest names."""
    with open(os.path.join(data_dir, "CURRENT")) as handle:
        generation = handle.read().strip()
    manifest = load(os.path.join(data_dir, "generations", generation, "state-manifest.json"))
    segments = [f for f in manifest["files"] if f["path"].endswith(".seg")]
    must(segments, "the live manifest names no segment")
    target = max(segments, key=lambda f: f["bytes"])
    path = os.path.join(data_dir, target["path"])
    with open(path, "r+b") as handle:
        data = bytearray(handle.read())
        offset = len(data) // 2
        data[offset] ^= 0x01
        handle.seek(0)
        handle.write(data)
    print("%s %d %d" % (target["path"], offset, len(data)))


MODES = {
    "ingest": ingest,
    "chain": chain,
    "head": head,
    "verify-live": verify_live,
    "recompute": recompute,
    "sign": sign,
    "anchored": anchored,
    "check-signature": check_signature,
    "erased": erased,
    "erasure-entry": erasure_entry,
    "evidence": evidence,
    "flip": flip,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](*sys.argv[2:])
