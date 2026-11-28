"""Captures the insight fixtures the main store answers (CAPTURED.md).

Run it against the server CAPTURED.md starts, after corpus.py, within 15
minutes of seeding: the behaviour corpus's live runs are live only that
long. Each response body is written into OUTDIR unedited. Standard library
only.

Usage: python3 -I capture.py http://127.0.0.1:8080 OUTDIR
"""

import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE, OUT = sys.argv[1], sys.argv[2]
CC = "11111111-1111-4111-8111-111111111111"
YEAR_BACK = str(time.time_ns() - 365 * 86400 * 10**9)


def save(name, path, params=None, want=200):
    url = BASE + path + ("?" + urllib.parse.urlencode(params) if params else "")
    try:
        with urllib.request.urlopen(url, timeout=120) as resp:
            status, body = resp.status, resp.read()
    except urllib.error.HTTPError as err:
        status, body = err.code, err.read()
    assert status == want, (name, status, body)
    with open(f"{OUT}/{name}", "wb") as f:
        f.write(body)
    print(f"{status} {name}", flush=True)
    return json.loads(body)


# The live runs first.
save("fate_run_doomed.json", "/v1/fate/runs/live-doomed-0")
save("fate_run_healthy.json", "/v1/fate/runs/live-healthy-0")
save("fate_run_novel.json", "/v1/fate/runs/live-novel-0")
save("fate_run_cc.json", f"/v1/fate/runs/{CC}")

save("fate_cohorts.json", "/v1/fate/cohorts")
save("fate_map_support.json", "/v1/fate/map", {"agent": "support-agent"})
save("fate_map_researcher.json", "/v1/fate/map", {"agent": "researcher"})
save("fate_map_support_depth2.json", "/v1/fate/map", {"agent": "support-agent", "depth": 2})
save("fate_map_support_min100.json", "/v1/fate/map", {"agent": "support-agent", "min_support": 100})
save("fate_map_missing.json", "/v1/fate/map", {"agent": "nobody"}, want=404)
save("fate_map_cc_pruned.json", "/v1/fate/map", {"agent": "claude-code", "since": "1760000000000000000"})

# An X-Ray is asked for by the call's span, which the autopsy names.
steps = {s["step_index"]: s["span"] for s in save("cache_fix7.json", "/v1/cache/fix-parser-7")["steps"]}
for step in (30, 34):
    save(f"xray_fix7_{step}.json", f"/v1/xray/{steps[step]['trace_id']}/{steps[step]['span_id']}")
steps = {s["step_index"]: s["span"] for s in save("cache_cc1.json", f"/v1/cache/{CC}")["steps"]}
save("xray_cc1_14.json", f"/v1/xray/{steps[14]['trace_id']}/{steps[14]['span_id']}")
save("xray_inject.json", "/v1/xray/1a7ec700000000000000000000000000/s3")

save("changes.json", "/v1/changes")
save("changes_run.json", "/v1/changes", {"run": "ticket-076"})

save("tql_tamper.json", "/v1/trajectories/search",
     {"q": 'test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]'})
save("tql_cc_year.json", "/v1/trajectories/search",
     {"q": "test[error] ..> edit", "since": YEAR_BACK, "agent": "claude-code"})
save("tql_explain.json", "/v1/trajectories/explain", {"q": "edit[file=@f] (..> edit[file=@f]){4,} within 10"})
save("tql_bad_field.json", "/v1/trajectories/search", {"q": "tool(bash)[stauts=error]"}, want=400)
save("detectors.json", "/v1/detectors")
save("findings.json", "/v1/detectors/findings")

save("blame_item.json", "/v1/blame", {"path": "src/item.rs", "line": "pub price: u64,"})
save("blame_test.json", "/v1/blame", {"path": "tests/test_parser.py", "line": "    assert parse('  Mixed ') == 'mixed'"})
save("blame_none.json", "/v1/blame", {"path": "src/cart.rs", "line": "nothing wrote this line"})

# A cursor carries its window: the second page names none, and a page that
# names a different one is refused.
cursor = save("tql_page1.json", "/v1/trajectories/search",
              {"q": "test[error] ..> edit", "since": YEAR_BACK, "limit": 1})["next_cursor"]
save("tql_page2.json", "/v1/trajectories/search", {"q": "test[error] ..> edit", "cursor": cursor, "limit": 1})
later = str(int(cursor.split(".")[1]) + 3600 * 10**9)
save("tql_cursor_window.json", "/v1/trajectories/search",
     {"q": "test[error] ..> edit", "cursor": cursor, "limit": 1, "since": later}, want=400)
