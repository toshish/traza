# Insight fixtures

Every file here is a response body a 0.28.0 `traza-server` returned, saved
unedited. The insight screens' tests render them, so a screen is tested
against what the server actually says, not against a shape someone typed.

## The store they came from

```sh
seed --data-dir ./data --behaviours
traza-server import claude-code --path tests/fixtures/import/claude-code --data-dir ./data --no-state
traza-server import codex --path tests/fixtures/import/codex --data-dir ./data --no-state
traza-server --data-dir ./data --port 8080 --pricing pricing.json
python3 ui/src/test/fixtures/insight/corpus.py http://127.0.0.1:8080
```

`pricing.json` named input, output, cache-read and cache-write rates for
`claude-sonnet-*`, `claude-haiku-*` and `gpt-4o*`, so the Cache Autopsy
prices what it loses. The behaviour corpus's six live runs are live only for
the 15 minutes after seeding; the `fate_run_*` fixtures were captured inside
that window, so their last step is open and pooled.

## Which request made which file

| File | Request |
|---|---|
| `fate_cohorts.json` | `GET /v1/fate/cohorts` |
| `fate_map_support.json`, `fate_map_researcher.json` | `GET /v1/fate/map?agent=support-agent` (and `researcher`) |
| `fate_map_support_depth2.json`, `fate_map_support_min100.json` | `GET /v1/fate/map?agent=support-agent&depth=2` (and `&min_support=100`): the same tree drawn short of where foresight judges the live runs |
| `fate_map_missing.json` | `GET /v1/fate/map?agent=nobody` (404) |
| `fate_map_cc_pruned.json` | `GET /v1/fate/map?agent=claude-code&since=1760000000000000000` (two runs, every branch under the default support) |
| `fate_run_doomed.json`, `fate_run_healthy.json`, `fate_run_novel.json` | `GET /v1/fate/runs/live-doomed-0` (`live-healthy-0`, `live-novel-0`) |
| `fate_run_cc.json` | `GET /v1/fate/runs/11111111-1111-4111-8111-111111111111` (the imported Claude Code session, older than the 30-day window) |
| `cache_fix7.json` | `GET /v1/cache/fix-parser-7` |
| `xray_fix7_30.json`, `xray_fix7_34.json` | `GET /v1/xray/{trace}/{span}` for steps 30 and 34 of `fix-parser-7` |
| `cache_cc1.json`, `xray_cc1_14.json` | the Claude Code session's autopsy, and the X-Ray of its step 14 |
| `xray_inject.json` | `GET /v1/xray/1a7ec700000000000000000000000000/s3` (`inject-1`) |
| `changes.json`, `changes_run.json` | `GET /v1/changes`, and `?run=ticket-076` |
| `tql_tamper.json` | `GET /v1/trajectories/search?q=test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]` |
| `tql_cc_year.json` | the same search for `test[error] ..> edit` with `since` a year back |
| `tql_explain.json` | `GET /v1/trajectories/explain?q=edit[file=@f] (..> edit[file=@f]){4,} within 10` |
| `tql_bad_field.json` | `GET /v1/trajectories/search?q=tool(bash)[stauts=error]` (400) |
| `detectors.json`, `findings.json` | `GET /v1/detectors`, `GET /v1/detectors/findings` |
| `blame_item.json`, `blame_test.json`, `blame_none.json` | `GET /v1/blame?path=src/item.rs&line=pub price: u64,`, `?path=tests/test_parser.py&line=    assert parse('  Mixed ') == 'mixed'`, and a line no edit wrote |
| `tql_page1.json`, `tql_page2.json` | `GET /v1/trajectories/search?q=test[error] ..> edit&since=<a year back>&limit=1`, then the same with `cursor=` the first page's `next_cursor` and no `since` |
| `tql_cursor_window.json` | that cursor sent with a `since` an hour later than the one it carries (400) |

## Captured from stores of their own

Each of these came from a fresh `traza-server --data-dir <empty>` given only
the spans described, so the case it shows is the only thing in it.

| File | Store | Request |
|---|---|---|
| `blame_unconfirmed.json` | Three `Edit` calls writing `let total = items.len();` into `/srv/shop/src/cart.rs`, one run each: one with status `error` and Claude Code's not-found result, one with no status and no result, one with status `ok` and a result | `GET /v1/blame?path=src/cart.rs&line=let total = items.len();` |
| `changes_params.json` | 120 runs of `tuned-bot`, whose `gen_ai.request.stop` once spelled out a temperature and whose `max_tokens` was text, then became `stop="END"`, `temperature=0.7` and `max_tokens=1024` (error rate 0% → 40%); and 120 runs of `triage-bot`, whose one-word prompt edit moved success from 8 of 8 ended runs to 0 of 8, every other run declaring `session.outcome=escalated` | `GET /v1/changes` |
| `cache_long.json` | Three calls of `long-ctx-1`: 2,000 one-line turns, the same with an instruction put ahead of them, and that with one more turn | `GET /v1/cache/long-ctx-1` |
| `xray_unread.json` | `--payload-threshold-bytes 200`; two calls of `unread-1` in one conversation, the first's two instructions inline, the second's request (the same two and a long question) offloaded | `GET /v1/xray/0ff10ad0000000000000000000000000/00000000000000d1?resolve=false` |
| `fate_map_loops.json`, `fate_map_mini.json` | Ten completed runs, three hours old: `loop-agent`'s `s1`…`s7` made one `gpt-4o` call then called `search` 1 to 7 times, and `cap3` called `Search` three times; `mini-agent`'s `lit` called a tool literally named `search×2` once and `two` called `search` twice, each between two `gpt-4o-mini` calls | `GET /v1/fate/map?agent=loop-agent&min_support=1` (and `mini-agent`) |
| `tql_loops.json` | The same store | For every node of `fate_map_loops.json`, `GET /v1/trajectories/search?q=…&agent=loop-agent&limit=100` with the query the dashboard's **Find these runs** sends, and the one the wave-3 dashboard sent; the file maps each query to its answer |
| `changes_tenants.json` | 60 runs each of `support-agent` in tenants `acme` and `globex`, `service.version` 1.0.0 then 1.1.0 (globex's error rate 7% → 33%) | `GET /v1/changes` in the operator view |

The Live screen's `../live_stream_removed.txt` is `GET /v1/runs/live/stream`
from a server run with `--fuse-max-runs 1 --live-window-seconds 3`: one run,
then a second that took the guard's one slot (the first `evicted`), then
three quiet seconds (the second `expired`).
