#!/usr/bin/env python3
"""The crash demo's ingest client, and its measurement helpers.

`stream` is the client: it POSTs batches of spans over one keep-alive
connection and counts a span as durable only when a 200 arrives carrying
{"accepted":N,"durability":"wal"}. It expects the connection to die under
it — that is the demo — and records exactly what was and was not
acknowledged when it did.

The other modes each read one measurement out of a JSON reply. They are
kept out of the shell script for the same reason mcp-demo keeps render.py:
inline `python3 -c` and shell quoting is where scripts like this go wrong.
"""

import http.client
import json
import os
import sys
import time


def span_id(i):
    return "span-%06d" % i


def trace_id(i):
    # Ten spans per trace, deterministically.
    return "crash-trace-%06d" % ((i - 1) // 10 + 1)


def stream(argv):
    """stream URL TOTAL BATCH PROGRESS_FIFO RESULT_FILE"""
    url, total, batch = argv[0], int(argv[1]), int(argv[2])
    fifo_path, result_path = argv[3], argv[4]
    assert url.startswith("http://"), url
    host, _, port = url[len("http://") :].split("/")[0].partition(":")

    # Opened first: the shell reads progress from this FIFO (it holds the
    # read side open read-write, so this open cannot block), and everything
    # after this line is allowed to fail without wedging it.
    progress = open(fifo_path, "w", buffering=1)

    base = time.time_ns()
    acked = 0
    last_span = None
    last_trace = None
    stopped = "completed: every batch was acknowledged"
    inflight_first = None
    inflight_size = 0
    connection = http.client.HTTPConnection(host, int(port or 80), timeout=30)

    def make_span(i):
        start = base + i * 1_000_000
        return {
            "trace_id": trace_id(i),
            "span_id": span_id(i),
            "parent_span_id": None,
            "name": "unit-of-work",
            "service": "crash-test",
            "status": "ok",
            "start_time_ns": start,
            "end_time_ns": start + 750_000,
            "attributes": {"seq": i},
        }

    try:
        i = 1
        while i <= total:
            hi = min(i + batch - 1, total)
            body = json.dumps([make_span(j) for j in range(i, hi + 1)]).encode()
            inflight_first, inflight_size = i, hi - i + 1
            try:
                connection.request(
                    "POST", "/v1/spans", body, {"Content-Type": "application/json"}
                )
                reply = connection.getresponse()
                payload = reply.read()
            except (OSError, http.client.HTTPException) as error:
                stopped = "connection lost mid-batch (%s)" % type(error).__name__
                break
            if reply.status != 200:
                print(
                    "ingest returned %d: %s" % (reply.status, payload[:200]),
                    file=sys.stderr,
                )
                sys.exit(2)
            ack = json.loads(payload)
            if ack.get("durability") != "wal":
                print(
                    "acknowledgement claims durability=%r, not wal"
                    % ack.get("durability"),
                    file=sys.stderr,
                )
                sys.exit(2)
            if ack.get("accepted") != inflight_size:
                print(
                    "accepted %r of a %d-span batch" % (ack.get("accepted"), inflight_size),
                    file=sys.stderr,
                )
                sys.exit(2)
            acked += inflight_size
            last_span, last_trace = span_id(hi), trace_id(hi)
            inflight_first, inflight_size = None, 0
            i = hi + 1
            try:
                progress.write("%d\n" % acked)
            except OSError:
                pass  # the reader is gone; keep streaming, the kill is coming
    finally:
        with open(result_path, "w") as handle:
            json.dump(
                {
                    "acknowledged": acked,
                    "last_span_id": last_span,
                    "last_trace_id": last_trace,
                    "first_unacked_seq": inflight_first,
                    "inflight_size": inflight_size,
                    "stopped": stopped,
                },
                handle,
            )
        try:
            progress.close()
        except OSError:
            pass


def field(argv):
    """field FILE KEY — print one scalar from a JSON file, shell-readably."""
    with open(argv[0]) as handle:
        value = json.load(handle)[argv[1]]
    if value is None:
        print("null")
    elif isinstance(value, bool):
        print(str(value).lower())
    else:
        print(value)


def export_count(argv):
    """export-count — parse a raw chunked /v1/export stream from stdin.

    Counts the NDJSON rows client-side, then reads the HTTP trailers and
    refuses to answer unless X-Traza-Export-Complete is true and
    X-Traza-Export-Count agrees with the client-side count. Prints the
    count on stdout; the cross-check detail goes to stderr for display.
    """
    raw = sys.stdin.buffer

    def line():
        out = bytearray()
        while True:
            byte = raw.read(1)
            if not byte:
                return bytes(out)
            out += byte
            if byte == b"\n":
                return bytes(out)

    rows = 0
    while True:
        size_line = line().strip()
        if not size_line:
            print("truncated chunked stream (no chunk size)", file=sys.stderr)
            sys.exit(1)
        size = int(size_line.split(b";")[0], 16)
        if size == 0:
            break
        chunk = b""
        while len(chunk) < size:
            more = raw.read(size - len(chunk))
            if not more:
                print("truncated chunk (%d of %d bytes)" % (len(chunk), size), file=sys.stderr)
                sys.exit(1)
            chunk += more
        rows += chunk.count(b"\n")
        raw.read(2)  # the CRLF after the chunk

    complete = None
    trailer_count = None
    while True:
        trailer = line().strip()
        if not trailer:
            break
        name, _, value = trailer.partition(b":")
        name, value = name.strip().lower(), value.strip()
        if name == b"x-traza-export-complete":
            complete = value
        elif name == b"x-traza-export-count":
            trailer_count = int(value)

    if complete != b"true":
        print("export incomplete: X-Traza-Export-Complete=%r" % complete, file=sys.stderr)
        sys.exit(1)
    if trailer_count != rows:
        print(
            "client counted %d rows but the trailer says %r" % (rows, trailer_count),
            file=sys.stderr,
        )
        sys.exit(1)
    print(
        "  %d rows counted client-side; trailer agrees "
        "(X-Traza-Export-Count: %d, X-Traza-Export-Complete: true)" % (rows, trailer_count),
        file=sys.stderr,
    )
    print(rows)


def stats(argv):
    """stats — one line of the fields this demo cares about, from stdin."""
    reply = json.load(sys.stdin)
    print(
        "  durability=%s buffered_records=%s persisted_records=%s "
        "segment_count=%s wal_bytes=%s"
        % tuple(
            reply.get(key)
            for key in (
                "durability",
                "buffered_records",
                "persisted_records",
                "segment_count",
                "wal_bytes",
            )
        )
    )


def corrupt(argv):
    """corrupt DATA_DIR — flip one byte in the middle of the largest segment."""
    data = argv[0]
    segments = sorted(
        (name for name in os.listdir(data) if name.startswith("segment-") and name.endswith(".seg")),
        key=lambda name: os.path.getsize(os.path.join(data, name)),
    )
    if not segments:
        print("no segment-*.seg files in %s" % data, file=sys.stderr)
        sys.exit(1)
    target = segments[-1]
    path = os.path.join(data, target)
    size = os.path.getsize(path)
    offset = size // 2
    with open(path, "r+b") as handle:
        handle.seek(offset)
        before = handle.read(1)[0]
        after = before ^ 0xFF
        handle.seek(offset)
        handle.write(bytes([after]))
        handle.flush()
        os.fsync(handle.fileno())
    print("%s offset=%d 0x%02x->0x%02x (file is %d bytes)" % (target, offset, before, after, size))


def verify(argv):
    """verify FILE intact|damaged [FILENAME] — print a /v1/verify reply, assert it."""
    path, expectation = argv[0], argv[1]
    named = argv[2] if len(argv) > 2 else None
    with open(path) as handle:
        reply = json.load(handle)
    print("  generation %s, intact: %s" % (reply["generation"], str(reply["intact"]).lower()))
    for problem in reply["problems"]:
        print("  problem: %s" % problem)
    if expectation == "intact":
        if reply["intact"] is not True or reply["problems"]:
            print("expected an intact store", file=sys.stderr)
            sys.exit(1)
    else:
        if reply["intact"] is not False:
            print("expected intact:false after the corruption", file=sys.stderr)
            sys.exit(1)
        if named and not any(named in problem for problem in reply["problems"]):
            print("no problem names the damaged file %s" % named, file=sys.stderr)
            sys.exit(1)


def has_span(argv):
    """has-span FILE SPAN_ID — exit 0 iff the trace reply contains the span."""
    with open(argv[0]) as handle:
        trace = json.load(handle)
    wanted = argv[1]
    sys.exit(0 if any(s.get("span_id") == wanted for s in trace.get("spans", [])) else 1)


MODES = {
    "stream": stream,
    "field": field,
    "export-count": export_count,
    "stats": stats,
    "corrupt": corrupt,
    "verify": verify,
    "has-span": has_span,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](sys.argv[2:])
