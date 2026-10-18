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
