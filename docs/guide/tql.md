# TQL: query agent behaviour like a regex

Traza holds every step of every run an agent made. **TQL** (trajectory query
language) asks whether a run's steps contain a *shape*: a failing test, then
an edit to a test file, then a passing test; the same file edited five times
within ten steps; a tool the model was never offered; two agents handing work
back and forth. It is a regular expression whose letters are steps.

```text
test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]
```

Read it as: *a test run that failed, then (eventually) an edit to a file
under `tests/`, then (eventually) a test run that passed.*

On top of the language sits a **detector library** — nine named patterns for
behaviours worth knowing about (test tampering, give-up loops, hallucinated
tools, edit thrash, agent ping-pong, blind retries, context blow-ups, error
streaks, unverified finishes) — and a findings report that runs the whole
library over a window and compares how the matching runs ended against the
rest.

Everything here is deterministic computation over stored spans. Traza never
calls a model to judge a run, and nothing leaves the process. Route shapes are
in the [HTTP API reference](http-api.md#trajectory-queries-and-detectors); the
agent-facing tool is [`find_behavior`](mcp.md#find_behavior).

- [See it in thirty seconds](#see-it-in-thirty-seconds)
- [Steps](#steps)
- [Atoms](#atoms)
- [Predicates](#predicates)
- [Fields](#fields)
- [Composing steps](#composing-steps)
- [Captures](#captures)
- [How a match is chosen](#how-a-match-is-chosen)
- [The regex subset](#the-regex-subset)
- [Grammar](#grammar)
- [Worked examples](#worked-examples)
- [Searching](#searching)
- [The detector library](#the-detector-library)
- [From an agent](#from-an-agent)
- [Limits, stated plainly](#limits-stated-plainly)

## See it in thirty seconds

A coding agent ran three sessions. In `tampered` it failed the tests, edited
`tests/test_login.py`, and passed; in `honest` it failed, edited
`src/login.py`, and passed; in `thrash` it edited `src/app.py` five times and
gave up. Which runs passed their tests by changing the tests?

```sh
curl -sG localhost:8080/v1/trajectories/search \
  --data-urlencode 'q=test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]' \
  --data-urlencode 'since=1772323199999999999'
```

```json
{
  "matches": [
    {
      "agent": "coder",
      "captures": {},
      "count": 1,
      "count_capped": false,
      "first_step": 1,
      "inexact": false,
      "last_activity_ns": 1772323205000000000,
      "last_step": 4,
      "outcome": "success",
      "run": "tampered",
      "run_truncated": false,
      "spans": [
        {
          "kind": "tool",
          "name": "Bash",
          "span_id": "tampered-1",
          "step_index": 1,
          "trace_id": "tr-tampered"
        },
        {
          "kind": "tool",
          "name": "Edit",
          "span_id": "tampered-3",
          "step_index": 3,
          "trace_id": "tr-tampered"
        },
        {
          "kind": "tool",
          "name": "Bash",
          "span_id": "tampered-4",
          "step_index": 4,
          "trace_id": "tr-tampered"
        }
      ],
      "spans_truncated": false,
      "steps": 6,
      "tenant": ""
    }
  ],
  "next_until_ns": null,
  "query": "test[error] ..> edit[file~\"(^|/)tests?/\"] ..> test[not error]",
  "runs_in_window": 3,
  "runs_scanned": 3,
  "since_ns": 1772323199999999999,
  "stopped": null,
  "truncated": false,
  "until_ns": null,
  "window_capped": false
}
```

`spans` are the steps the pattern's atoms matched — the failing test (step 1),
the edit (step 3), the passing test (step 4). Step 2, a model call the `..>`
skipped, is not listed. Step indices are the run's
[tape](replay.md#runs-and-tapes) indices, so `GET /v1/tapes/tampered` shows
each one in full.

Always URL-encode the query (`curl -G --data-urlencode`). An unencoded `+`
arrives as a space, so `tool(bash)+` would be read as `tool(bash)`; the
response's `query` field shows the pattern exactly as it was read, so a
surprise is visible.

## Steps

A run's **steps** are its spans whose step kind is `llm`, `tool` or `agent`
([step kinds](../llm-semantics.md#step-kind)), in step order
(`start, trace, span`) — exactly the steps of its tape, with the same indices.
Workflow wrappers, retrievals and HTTP spans are not steps. A run is a session
id, or `trace:<trace_id>` for a span with none
([run identity](../llm-semantics.md#run-identity)).

A pattern is matched against that sequence. An atom matches one step; the
operators combine atoms the way a regex combines characters.

## Atoms

| Atom | Matches |
|---|---|
| `step`, `step(NAME)` | any step; `NAME` is its tool, model or agent name |
| `tool`, `tool(NAME)` | a tool step; `NAME` is its tool name |
| `llm`, `llm(MODEL)` | a model call; `MODEL` is its model (response model, else request model) |
| `agent`, `agent(NAME)` | an agent invocation; `NAME` is its agent name |
| `edit`, `edit(PATH)` | a tool step that edits a file (see [`edit`](#derived-fields)); `PATH` is tested against every file it names |
| `test` | a tool step whose input runs a test suite (see [`test`](#derived-fields)); takes no name |

A **name** is one of:

| Form | Example | Means |
|---|---|---|
| bare word | `tool(bash)` | equal, ignoring ASCII case — `Bash` matches |
| quoted | `tool("str_replace_editor")` | the same, for names with other characters |
| regex | `tool(/^mcp__/)` or `tool(~"^mcp__")` | the regex finds a match in the name |
| capture | `tool(@t)` | captures the name (see [captures](#captures)) |
| any | `tool(*)` | any name — the same as `tool` |

Alternatives are separated by `|`: `tool(grep|glob|read)`. When a step has no
tool, model or agent name, its span name stands in.

## Predicates

Square brackets after an atom (or inside a [gap](#gaps)) test the step:

```text
tool(bash)[args~"pytest"]
step[status=error]
llm[cost>0.5]
step[duration>30s]
tool[result~"(?i)permission denied"]
edit[file~"test_"]
```

Several brackets must all hold. Inside one bracket, conditions combine with
`and` (or a comma), `or`, `not`, and parentheses; `not` binds tightest, then
`and`, then `or`: `[error or status=timeout, cost>0.1]` is
`[(error or status=timeout) and cost>0.1]`.

A condition is a field alone (true when present and truthy: non-empty text, a
non-zero number, `true`, a non-empty list) or `field OP value`:

| Operator | Means |
|---|---|
| `=`, `!=` | equal / not equal (`==` is accepted for `=`) |
| `~`, `!~` | the [regex](#the-regex-subset) finds a match / finds none |
| `>`, `>=`, `<`, `<=` | numeric comparison |

Values are a quoted string (`"…"` or `'…'`), a bare word (`error`,
`src/main.rs`, `gpt-4o`), a number (`0.5`, `-3`), a duration (`500ms`, `30s`,
`2m`, `1h`, `1d`; also `ns`, `us`), `true` / `false`, a regex (`/…/` or
`/…/i` after `~`; a string after `~` is a regex too), a capture (`@f`), or a
scaled capture (`2*@c`).

In a quoted string, `\\`, `\"`, `\'`, `\n`, `\t` and `\r` are escapes and any
other backslash is kept as written, so a regex's `\d` or `\.` reaches the
regex engine as typed.

**Missing values.** A comparison with a field the step does not have is
false — `[cost>0.5]` does not match a step that reported no cost — and `!=`
and `!~` are its negation, so `[file!="a.py"]` matches a step with no file at
all. A list field (`file`, `calls`, `offered`, `unknown_calls`) matches `=`
or `~` when **any** of its values does; `!=` when none does.

**Types are checked when the query is parsed.** `~` takes text (`cost~"1"` is
refused), ordering takes numbers (`status>3` is refused), `duration` takes a
unit (`duration>30` is refused — write `30s`), and booleans are tested bare,
with `not`, or with `=true` / `=false`.

## Fields

| Field | Type | Meaning |
|---|---|---|
| `kind` | text | `llm`, `tool` or `agent` |
| `name` | text | the tool, model or agent name (else the span name) |
| `span` | text | the span's operation name |
| `service` | text | the emitting service |
| `agent` | text | the agent the step ran under: its own agent name, else the nearest enclosing span's that names one |
| `model` | text | the model, when the span names one |
| `tool` | text | the tool name, for tool steps |
| `status` | text | the span status as recorded |
| `error` | bool | the status is `error` (any case) |
| `cost` | number | USD, metered by the span or priced from the server's [rates](../configuration.md#model-pricing) |
| `duration` | duration | end minus start |
| `tokens` | number | total tokens reported |
| `input_tokens`, `output_tokens` | number | prompt / completion tokens reported |
| `cache_read`, `cache_write` | number | prompt tokens read from / written to a provider cache |
| `context` | number | the context the call carried, where that can be told ([why it is not just input tokens](../llm-semantics.md#prompt-caching-and-what-prompt-tokens-means)) |
| `index` | number | the step's position in the run, from 0 |
| `args` (alias `input`) | text | the step's input as text |
| `result` (alias `output`) | text | the step's output as text |
| `args.<path>`, `result.<path>` | dynamic | a value inside the input / output, by key and array index: `args.command`, `args.edits.0.new_string` |
| `attr.<key>` | dynamic | any span attribute, verbatim: `attr.http.status_code`, `attr."key with spaces"` |
| `file` | list | paths named by a tool step's arguments |
| `digest` | text | SHA-256 of the step's stored input — equal digests, identical input |
| `fingerprint` | text | `kind:name#hash`, the repeat identity the guard uses |
| `signature` | text | `kind:name` |
| `calls` | list | tool names an llm step's response asked for |
| `offered` | list | the tool catalog an llm step offered; on a tool step, its requesting llm step's |
| `in_catalog` | bool | a tool step's name is in its requesting llm step's catalog — **absent** when that catalog is unknown |
| `unknown_calls` | list | calls an llm step made to tools outside its own catalog — absent when the catalog is unknown |
| `edit` | bool | a tool step that edits a file |
| `test` | bool | a tool step whose input runs a test suite |

**Text fields.** `args` and `result` are the stored value as text: a string as
itself, a scalar as its JSON text, a structure as its string leaves in
document order (object keys sorted), one per line. `^` and `$` in a regex
anchor at line boundaries, so `args~"^pytest"` matches a command leaf that
starts with `pytest`. A dynamic field yields text, a number, a boolean, or —
for an array of scalars — a list.

### Derived fields

These are heuristics, documented so their edges are known:

- **`file`** — the string values of the keys `file_path`, `filePath`, `path`,
  `file`, `filename`, `target_file`, `notebook_path` and `absolute_path`
  anywhere in a tool step's arguments, plus the file headers of a patch
  (`*** Update File:`, `*** Add File:`, `*** Delete File:`, `*** Move to:`,
  `+++ b/`). At most 512 per step.
- **`edit`** — a tool step that names at least one file and either is one of
  the editing tools `Edit`, `MultiEdit`, `Write`, `NotebookEdit`,
  `apply_patch`, `str_replace_editor`, `str_replace_based_edit_tool`,
  `edit_file`, `write_file`, `create_file`, `replace_in_file`,
  `write_to_file`, `apply_diff`, `search_replace`, `insert_edit_into_file`
  (any case), or carries new content under `new_string`, `new_str`, `content`,
  `contents`, `file_text`, `edits`, `patch`, `diff`, `code_edit` or
  `new_source`, or contains a patch — and whose `command` argument, if any, is
  not `view` or `read`.
- **`test`** — a tool step whose `args` text matches a test-runner invocation:
  `pytest`, `py.test`, `tox`, `nox`, `jest`, `vitest`, `mocha`, `rspec`,
  `phpunit`, `ctest`, `go test`, `cargo test`, `cargo nextest`,
  `npm|yarn|pnpm [run] test`, `bun test`, `deno test`,
  `python -m pytest|unittest`, `mvn … test`, `gradle … test`,
  `make … test|check`, `dotnet test`, `swift test`, `mix test`, `rake test`,
  `bundle exec rspec|rake`. A test run "fails" when its span status is
  `error` — which is what a shell tool's span records for a non-zero exit
  only when its instrumentation maps the exit code to the status.
- **`calls`** — entries of a `tool_calls` list (`name` or `function.name`),
  content blocks typed `tool_call`, `tool_use` or `function_call`, and a
  legacy `function_call` object, anywhere in an llm step's response.
- **`offered`** — `gen_ai.tool.definitions` (a list, or its JSON text, of
  `{name}` or `{function: {name}}`), OpenLLMetry's
  `llm.request.functions.{i}.name`, or OpenInference's
  `llm.tools.{i}.tool.json_schema`. A tool step's *requesting* llm step is its
  parent span when that is a model call, else the latest model call before it.
- **`agent`** — the step's own agent name ([precedence](../llm-semantics.md#tool-agent-and-call-id)),
  else the nearest ancestor span (up to 64 levels, within the run) that names
  one.

## Composing steps

| Syntax | Means |
|---|---|
| `A -> B` | `B` is the very next step after `A` |
| `A ..> B` | `B` comes later: any steps in between (as few as possible) |
| `A ..[pred]> B` | later, and every step in between satisfies `pred` |
| `A ..{n,m}> B` | later, with `n` to `m` steps in between (`{,m}`, `{n,}`, `{n}` too) |
| `A ..[pred]{n,m}> B` | both |
| `A (..> B){4,}` | a group that **starts with a link** continues the step before it — here, `A` then four or more later `B`s |
| `X*` `X+` `X?` `X{n}` `X{n,}` `X{n,m}` | repetition of adjacent matches; greedy |
| `X*?` `X+?` `X??` `X{n,m}?` | the same, lazy |
| `A \| B` | either; the left one is preferred |
| `( … )` | grouping |
| `^A`, `B$` | `A` is the run's first step; `B` is its last |
| `… within 10` / `within 10 steps` | the whole match covers at most 10 steps, gap steps included |
| `… within 5m` | the whole match's last step starts at most 5 minutes after its first |

`|` has the lowest precedence, then the links, then quantifiers:
`tool(a) -> tool(b) | llm` is `(tool(a) -> tool(b)) | llm`.
[`/v1/trajectories/explain`](#explain) renders any query with its grouping
made explicit.

### Gaps

The steps a `..>` skips are its *gap*. A gap predicate restricts them:
`tool(a) ..[kind=llm]> tool(b)` says only model calls may come between. Gap
predicates test and never [capture](#captures). Gap steps are part of the
match's range (`first_step`..`last_step`, and `within`) but are not listed in
its `spans`.

### What cannot be written

A pattern that can match zero steps — `tool?`, `step*`, `^` — would match
every run and is refused. So is a repetition of something that can match zero
steps (`(tool?)+`): it is ambiguous and never useful.

## Captures

`@name` captures a value the first time it is met and requires the same value
wherever it appears again:

```text
edit[file=@f] ..> edit[file=@f]            the same file edited twice
tool(@t)[error] -> tool(@t)                a failing tool called again immediately
agent(@a) ..> agent(@b)[name!=@a] ..> agent(@a)   A, then another agent, then A again
```

A capture is **bound** by `field=@name` in a bracket's top-level `and` chain,
or by a lone `@name` in an atom's names (`tool(@t)`). Everywhere else — under
`or` or `not`, in a gap, with `!=`, `~` or an ordering, or as one alternative
among several names — it is only **compared**, and a comparison with a capture
not yet bound is false (so its `!=` is true). A query that references a
capture nothing can bind is refused, naming the column.

A capture on a list field binds each value in turn: `edit[file=@f]` on a patch
naming two files tries both. A scaled capture compares numbers:
`llm[context=@c] ..> llm[context>=2*@c]`.

Captures are reported with each match (`"captures": {"f": "src/app.py"}`).
They are producer data.

## How a match is chosen

TQL uses **leftmost-first** semantics, the rule Perl, PCRE and RE2 share:

1. The match that **starts at the earliest step** wins.
2. Among matches from that step, the one a backtracking matcher would find
   first: alternatives left to right, quantifiers taking as many repetitions
   as they can (as few, when lazy), and gaps skipping as few steps as they
   can.

So `tool(read)+ -> edit` over `read read read edit` reports all three reads,
while `tool(read)+? ..> edit` reports one read and the edit.

After a match, the search resumes at the step after it, so the occurrences
counted in a run (`count`) never overlap.

Under the hood the pattern compiles to an NFA run by a Pike VM — every live
path through the pattern advances in lock step, one step at a time, in
priority order, and two paths that reach the same point with the same
captures keep only the higher-priority one. That is what keeps the cost at
*steps × live paths* rather than exponential, and why the answer equals the
backtracking one exactly. The test suite checks that: thousands of random
patterns over random runs, matched both ways, must agree on every match,
every listed step and every capture.

## The regex subset

`~` uses a small, std-only regex engine that runs in linear time (a Thompson
NFA simulated breadth-first), so no pattern can make it backtrack
exponentially.

| Syntax | Meaning |
|---|---|
| `abc` | literal characters |
| `.` | any character except a newline |
| `[abc]` `[a-z]` `[^…]` | character class, ranges, negation |
| `\d \w \s` / `\D \W \S` | digit, word character, whitespace / complements (also inside classes) |
| `*` `+` `?` `{n}` `{n,}` `{n,m}` | repetition (counts up to 256); a trailing `?` is accepted |
| `a\|b` | alternation |
| `( … )` `(?: … )` | grouping (no capture groups) |
| `^` `$` | start / end of a **line** |
| `\A` `\z` | start / end of the whole text |
| `\b` `\B` | word boundary / not a word boundary |
| `(?i)` | at the very start: case-insensitive (ASCII and simple Unicode folding); `/…/i` is the same |
| `\n \t \r \f \v` `\xHH` `\u{H…}` | control characters and code points |
| `\` + punctuation | that character literally |

Refused, with the column: backreferences, lookaround, `(?…)` groups other
than `(?:`, inline flags other than a leading `(?i)`, POSIX classes
(`[:alpha:]`), and Unicode classes (`\p{…}`). A regex is at most 1024
characters and 4096 compiled instructions.

## Grammar

```ebnf
query       = pattern [ "within" bound ] ;
bound       = INTEGER [ "steps" | "step" ] | DURATION ;

pattern     = sequence { "|" sequence } ;
sequence    = [ "^" ] term { link term | continuation } [ "$" ] ;
link        = "->" | gap ;
gap         = ".." [ "[" predicate "]" ] [ range ] ">" ;
continuation= "(" cont_seq { "|" cont_seq } ")" [ quantifier ] ;
cont_seq    = link term { link term } [ "$" ] ;
term        = primary [ quantifier ] ;
quantifier  = ( "*" | "+" | "?" | range ) [ "?" ] ;
range       = "{" INTEGER "}" | "{" INTEGER "," [ INTEGER ] "}"
            | "{" "," INTEGER "}" ;               (* ",m" form: gaps only *)
primary     = atom { "[" predicate "]" } | "(" pattern ")" ;
atom        = kind [ "(" name { "|" name } ")" ] ;
kind        = "step" | "tool" | "llm" | "agent" | "edit" | "test" ;
name        = WORD | STRING | REGEX | "~" STRING | CAPTURE | "*" ;

predicate   = conjunction { "or" conjunction } ;
conjunction = negation { ( "and" | "," ) negation } ;
negation    = "not" negation | "(" predicate ")" | condition ;
condition   = field [ operator value ] ;
field       = WORD [ STRING ] ;                    (* attr.KEY, args.a.b, attr."x y" *)
operator    = "=" | "==" | "!=" | "~" | "!~" | ">" | ">=" | "<" | "<=" ;
value       = STRING | REGEX | WORD | NUMBER | DURATION | "true" | "false"
            | CAPTURE | NUMBER "*" CAPTURE | CAPTURE "*" NUMBER ;
```

Tokens:

| Token | Form |
|---|---|
| `WORD` | a letter or `_`, then letters, digits, `_`, `:`, `/`, `-` (not before `>`) and `.` (not before `.`) |
| `STRING` | `"…"` or `'…'` with the escapes above |
| `REGEX` | `/…/` (a `\/` inside is a `/`), optionally followed by `i` |
| `NUMBER` | digits with an optional fraction and leading `-` |
| `DURATION` | a non-negative number immediately followed by `ns`, `us`, `ms`, `s`, `m`, `h` or `d` |
| `CAPTURE` | `@` then a name (`[A-Za-z_][A-Za-z0-9_]*`) |

Keywords (`and`, `or`, `not`, `within`, `true`, `false`) and step kinds are
case-insensitive; fields are lower-case. Whitespace is free. Counts in
quantifiers and gap bounds are at most 100; a query is at most 4096
characters, 256 atoms, 16 captures, 64 levels of nesting, and 20,000
compiled instructions.

**Every refusal names its column** (1-based, in characters) and, for an
unknown step kind or field, the likely intended one:

```sh
curl -sG localhost:8080/v1/trajectories/search \
  --data-urlencode 'q=tool(bash)[stauts=error]'
```

```json
{"column":12,"error":"column 12: unknown field `stauts`; did you mean `status`? Fields: kind, name, span, service, agent, model, tool, status, error, cost, duration, tokens, input_tokens, output_tokens, cache_read, cache_write, context, index, args, result, file, digest, fingerprint, signature, calls, offered, in_catalog, unknown_calls, edit, test, plus args.<path>, result.<path> and attr.<key>"}
```

## Worked examples

**1. Every failing tool call.**
`tool[error]` — one occurrence per failing tool step.

**2. One tool failing.** `tool(bash)[error]` — names ignore ASCII case, so this
matches Claude Code's `Bash`.

**3. Output containing a phrase.** `tool[result~"(?i)permission denied"]`.

**4. A slow step.** `step[duration>30s]`.

**5. An expensive model call.** `llm[cost>0.5]` — metered cost, or priced from
the server's rates.

**6. A model family over a budget.** `llm(/^gpt-4o/)[tokens>20000]`.

**7. A dangerous command, by argument.** `tool(bash)[args.command~"^rm -rf"]`.

**8. Any raw attribute.** `step[attr.http.status_code>=500]`.

**9. Tests failed, later passed.** `test[error] ..> test[not error]`.

**10. Test tampering, your own way.**
`test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]` — the library's
`test_tampering` adds a guard: no passing test between the failure and the
edit.

**11. The same file edited twice.** `edit[file=@f] ..> edit[file=@f]` —
`captures.f` names the file.

**12. Edit thrash.** `edit[file=@f] (..> edit[file=@f]){4,} within 10` — five
edits of one file inside ten steps.

**13. A retry storm, back to back.** `tool(@t)[error] (-> tool(@t)[error]){2,}`
— the same tool failing three or more times in a row.

**14. A blind retry.** `step[error, digest=@d] (..[kind=llm]> step[digest=@d]){1,}`
— a failed request sent again byte-for-byte, with only model calls between.

**15. A tool that was never offered.** `tool[in_catalog=false]` — written with
`=false` on purpose: `[not in_catalog]` would also match steps whose catalog
is unknown.

**16. The model asked for a tool outside its catalog.** `llm[unknown_calls]`.

**17. Read, then immediately edit, the same file.**
`tool(read)[file=@f] -> edit[file=@f]`.

**18. A handoff and back.** `agent(planner) ..> agent(coder) ..> agent(planner)`.

**19. Ping-pong between any two agents.**
`agent(@a) (..[kind!=agent]> agent(@b)[name!=@a] ..[kind!=agent]> agent(@a)){3,}`
— A→B→A three times, no third agent in between.

**20. The run ended on an error.** `step[error]$`.

**21. The run started with a huge context.** `^llm[context>50000]`.

**22. Context doubling.** `llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6`
— the second call at most five steps after the first.

**23. Only model calls between two searches.**
`tool(search) ..[kind=llm]> tool(search)`.

**24. A rollback soon after a deploy.** `tool(deploy) ..> tool(rollback) within 10m`.

**25. An exploration spree.** `tool(grep|glob|read){5,}` — five or more
searches and reads in a row.

**26. Gave up.** `tool(@t)[error] (..[not (name=@t and not error)]> tool(@t)[error]){2,} ..[not (name=@t and not error)]> llm$`
— three failures of one tool, never a success of it, and the run ends on a
model answer. (This is the library's `give_up_loop`.)

**27. Finished without testing.** `edit ..[not test]> llm$`.

**28. Greedy against lazy.** Over `read read read edit`,
`tool(read)+ -> edit` lists all three reads; `tool(read)+? ..> edit` lists the
first read and the edit (the other two are gap steps).

## Searching

```text
GET /v1/trajectories/search?q=&since=&until=&agent=&outcome=&limit=&run=&tenant=
```

| Parameter | Default | Means |
|---|---|---|
| `q` | required | the TQL query |
| `since`, `until` | the last 30 days | the window, in Unix nanoseconds (`since_ns` / `until_ns` also accepted): runs with a span starting in it |
| `agent` | any | only runs of this agent: the run's first step's agent name, else its earliest span's service |
| `outcome` | `any` | `success`, `failure`, `abandoned`, `unknown` — how the run ended, by the [session outcome](http-api.md#get-v1recall) rule |
| `limit` | 50 | matching runs returned, at most 500 |
| `run` | — | search this one run and list every occurrence in it; the window and `agent` do not apply. In the operator view a run id two tenants share is read as their union, as a [tape](replay.md#runs-and-tapes) is — name the `tenant` |
| `tenant` | — | for an unbound credential: one tenant (empty is the default tenant); omitted is the operator view across tenants, each run read within its own tenant |

The scan reads the window's **most recent runs first**: it enumerates the runs
with activity in the window (keeping the 5,000 most recent), then reads each
run whole — every step, not just those inside the window — and matches it,
until `limit` runs have matched. The response:

| Field | Means |
|---|---|
| `query` | the query as read (canonical form) |
| `matches[]` | matching runs, newest first: `run`, `tenant`, `agent`, `outcome`, the first occurrence's `spans` (`trace_id`, `span_id`, `step_index`, `kind`, `name`, at most 100), `captures`, `first_step`, `last_step`, `count` (non-overlapping occurrences, at most 1,000), `steps`, `last_activity_ns`, `run_truncated`, `inexact`; with `run=`, also `occurrences[]` (at most 100) |
| `runs_scanned` | runs read and matched |
| `runs_in_window` | candidate runs (at most 5,000) |
| `stopped` | `"limit"` or `"deadline"` when the scan stopped before the last candidate, else `null` |
| `window_capped` | the window held more than 5,000 runs; only the most recent were considered |
| `truncated` | there may be matches this answer does not list (any of the above) |
| `next_until_ns` | when the scan stopped early: the latest activity of the newest run it did not check — repeat the search with `until` set to it to continue (a run active on both sides of that instant can appear in both answers) |

**One compute budget** (`--query-deadline-ms`) covers the request.
Enumerating the window's runs refuses with `400` when it runs out — narrow the
window. Scanning stops where the budget ran out and answers what it found with
`"stopped": "deadline"`, `"truncated": true` and a `next_until_ns` to resume
from: the runs it did not reach are older than every run it did.

### Explain

`GET /v1/trajectories/explain?q=` parses and compiles a query without touching
the store — for a query box that validates as you type:

```sh
curl -sG localhost:8080/v1/trajectories/explain \
  --data-urlencode 'q=edit[file=@f] (..> edit[file=@f]){4,} within 10'
```

```json
{
  "atoms": 2,
  "captures": [
    "f"
  ],
  "fields": [
    "file"
  ],
  "instructions": 24,
  "max_steps": null,
  "min_steps": 5,
  "query": "edit[file=@f] (..> edit[file=@f]){4,} within 10",
  "within": {
    "steps": 10
  }
}
```

## The detector library

`GET /v1/detectors` lists them. Each is an ordinary TQL query — paste it into
a search to see the same matches, or adapt its threshold.

| Detector | Severity | Finds | Query |
|---|---|---|---|
| `test_tampering` | high | A test run fails, a test file is edited before any test passes, and a later test run passes | `test[error] ..[not (test and not error)]> edit[file~"<test file>"] ..> test[not error]` |
| `hallucinated_tool` | high | A tool step outside its requesting model call's catalog, or a model call asking for a tool outside its own — only where the catalog was recorded | `tool[in_catalog=false] \| llm[unknown_calls]` |
| `give_up_loop` | medium | One tool fails ≥ 3 times with no success of it in between, and the run ends on a model answer | see example 26 |
| `edit_thrash` | medium | One file edited ≥ 5 times within 10 steps | `edit[file=@f] (..> edit[file=@f]){4,} within 10` |
| `ping_pong` | medium | A→B→A agent handoffs ≥ 3 times, no other agent in between | see example 19 |
| `context_blowup` | medium | A model call's context (≥ 1000 tokens) at least doubled by a call ≤ 5 steps later | `llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6` |
| `retry_without_change` | low | A failed request repeated byte-for-byte, only model calls between | see example 14 |
| `tool_error_streak` | low | ≥ 5 failing tool calls with only model calls between | `tool[error] (..[kind=llm]> tool[error]){4,}` |
| `unverified_finish` | low | No test run after the last edit, and the run ends on a model answer | `edit ..[not test]> llm$` |

A *test file* is a path under a `test`, `tests`, `spec` or `__tests__`
directory, or named `test_*`, `*_test.*`, `*.test.*` or `*.spec.*`.

### Findings

```text
GET /v1/detectors/findings?since=&until=&agent=&detector=&tenant=
```

runs the library (or the detectors named — `detector=a,b` or repeated) over
the window's 2,000 most recent runs, and reports per detector the runs and
occurrences it matched, how those runs ended, their `success_rate`, up to five
example runs (newest first), and when it last fired — next to the `baseline`:
how every scanned run ended. A detector whose matched runs succeed far less
often than the baseline is a behaviour costing you outcomes. Detectors with
findings come first, by severity, then by runs matched.

```sh
curl -sG localhost:8080/v1/detectors/findings \
  --data-urlencode 'since=1772323199999999999' \
  --data-urlencode 'detector=test_tampering,edit_thrash'
```

Abridged — each example run is cut to `run`, `outcome` and `captures`, and each detector's `query` and `summary` (the same as in `/v1/detectors`) are left out:

```json
{
  "baseline": {
    "abandoned": 0,
    "failure": 1,
    "success": 2,
    "unknown": 0
  },
  "baseline_success_rate": 0.6666666666666666,
  "detectors": [
    {
      "examples": [
        {
          "run": "tampered",
          "outcome": "success",
          "captures": {}
        }
      ],
      "last_seen_ns": 1772323205000000000,
      "name": "test_tampering",
      "occurrences": 1,
      "outcomes": {
        "abandoned": 0,
        "failure": 0,
        "success": 1,
        "unknown": 0
      },
      "runs_matched": 1,
      "severity": "high",
      "success_rate": 1.0,
      "title": "Edited the tests to make them pass"
    },
    {
      "examples": [
        {
          "run": "thrash",
          "outcome": "failure",
          "captures": {
            "f": "src/app.py"
          }
        }
      ],
      "last_seen_ns": 1772323410000000000,
      "name": "edit_thrash",
      "occurrences": 1,
      "outcomes": {
        "abandoned": 0,
        "failure": 1,
        "success": 0,
        "unknown": 0
      },
      "runs_matched": 1,
      "severity": "medium",
      "success_rate": 0.0,
      "title": "Edited one file over and over"
    }
  ],
  "next_until_ns": null,
  "runs_in_window": 3,
  "runs_scanned": 3,
  "since_ns": 1772323199999999999,
  "stopped": null,
  "truncated": false,
  "until_ns": null,
  "window_capped": false
}
```

`success_rate` counts runs whose outcome is known: successes over successes,
failures and abandonments. A run still active, or longer than the span budget,
is `unknown` and is in neither side of the rate.

## From an agent

The MCP tool [`find_behavior`](mcp.md#find_behavior) takes a `query` or a
`detector` (or neither, to list the library), plus `since`, `until`, `agent`,
`outcome` and `limit`, and returns each match's run, outcome and matched
steps — run ids, names and captured values inside the untrusted-telemetry
block, a parse error with a caret under the column.

## Limits, stated plainly

- **Heuristics are heuristics.** `edit`, `test`, `file` and the test-file
  pattern read tool names, argument keys and command text. A test runner
  invoked through a wrapper script (`./ci.sh`) is not a `test`; a tool that
  writes files under an unknown name and unknown keys is not an `edit`. The
  lists are above; match the raw fields (`tool(...)`, `args.<path>`) when
  yours differ.
- **Failure means span status.** A test run counts as failed only when its
  span says `error`. A runner that exits 0 on failure, or instrumentation that
  never marks errors, makes `test[error]` match nothing.
- **Catalogs are optional.** `in_catalog`, `unknown_calls` and
  `hallucinated_tool` say nothing when the model call did not record its tool
  definitions — most instrumentation does not by default.
- **Text is capped.** `args` and `result` are matched on their first 64 KiB.
  A value ingest [offloaded](data-model.md#payload-references) to the payload store is its
  reference and inline preview (256 characters), not its full text — the same
  rule as content search.
- **Runs are read whole, windows pick them.** The window selects runs by span
  start time; a selected run is then matched over all its steps, up to 20,000
  spans (past that, its beginning is matched and `run_truncated` is set; its
  outcome is then `unknown`).
- **The agent filter is applied after recency.** With `agent=`, the scan still
  considers the window's 5,000 most recent runs of any agent and skips the
  others; in a busy multi-agent store, narrow the window to reach older runs of
  one agent.
- **Partial answers say so.** A scan stopped by the deadline or the limit sets
  `truncated` and `stopped`. Enumerating the window is all-or-nothing.
- **Path cap.** The matcher keeps at most 4,096 live paths per step. A
  capture-heavy pattern over a long run with thousands of distinct values
  (one path per file waiting for its pair) can exceed it; the lowest-priority
  paths are dropped and the run is marked `inexact`.
- **No index.** A search reads every span in its window once, to find the
  window's runs, and then each candidate run in full — work proportional to
  the window's spans plus the scanned runs' spans, under the query deadline.
  Narrow windows and a `limit` keep it fast; it is not built to
  scan millions of runs in one request.
- **Names ignore ASCII case; `=` does not.** `tool(bash)` matches `Bash`,
  `[tool=bash]` does not.
- **Not expressible:** "never" over the whole run (negative lookbehind, "a
  file edited without ever being read before"), arithmetic beyond a scaled
  capture, and comparisons between two different fields of one step.
