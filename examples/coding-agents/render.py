#!/usr/bin/env python3
"""Renders and asserts one step of the coding-agents demo.

Kept out of the shell script for the reason the other demos keep theirs:
quoting JSON inside `python3 -c` is where scripts like this go wrong. Every
mode prints what the server answered and exits non-zero when it is not what
the demo claims. Costs are recomputed from the transcripts' own token counts
and the demo's pricing file, so a server figure is checked against
arithmetic that never touched the server.

Text a transcript recorded — previews, prompts, paths — is producer data:
it is printed with control characters replaced, so a transcript cannot
drive the terminal, and with the project's absolute path shown relative.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path) as handle:
        return json.load(handle)


PROJECT = ""


def tidy(text, limit=None):
    """Producer text, safe for a terminal and short enough for a row: control
    characters replaced, runs of whitespace folded to one space."""
    text = "".join(c if c.isprintable() else " " for c in str(text))
    if PROJECT:
        text = text.replace(PROJECT + "/", "")
        if text.endswith("…"):
            # A preview cut inside the project's path: drop the partial path.
            body = text[:-1]
            for k in range(len(PROJECT), 3, -1):
                if body.endswith(PROJECT[:k]):
                    text = body[:-k] + "…"
                    break
    text = " ".join(text.split())
    if limit and len(text) > limit:
        text = text[:limit - 1] + "…"
    return text


def money(value):
    return "$%.4f" % value


def bar(share, width=24):
    full = share * width
    return "█" * int(full) + ("▌" if full - int(full) >= 0.5 else "")


def manifest(path):
    global PROJECT
    doc = load(path)
    PROJECT = doc["project"]
    return doc


def short(run):
    return run[:8]


# ------------------------------------------------------------------ import


def imported(manifest_path, claude_path, codex_path, seconds):
    manifest(manifest_path)
    claude, codex = load(claude_path), load(codex_path)
    for counts in (claude, codex):
        must(counts["files_failed"] == 0 and counts["spans_refused"] == 0 and not counts["errors"],
             "the import refused something: %s" % counts)
        must(counts["transcript"]["malformed_lines"] == 0 and not counts["transcript"]["unknown_records"]
             and counts["transcript"]["unknown_blocks"] == 0,
             "every synthetic record should be one the importer understands: %s" % counts["transcript"])
    # What transcripts.py wrote: 27 Claude Code responses and 20 tool calls
    # over 3 sessions and 2 sub-agents; 4 Codex responses and 3 tool calls.
    must((claude["sessions"], claude["llm_steps"], claude["tool_steps"], claude["agent_spans"],
          claude["tool_errors"]) == (3, 27, 20, 2, 1), "the Claude Code counts: %s" % claude)
    must((codex["sessions"], codex["llm_steps"], codex["tool_steps"]) == (1, 4, 3),
         "the Codex counts: %s" % codex)
    print("    %-12s %5s %8s %6s %5s %5s %6s %11s %10s %11s" % (
        "source", "files", "sessions", "spans", "llm", "tool", "agent", "tool errors", "tokens in",
        "cache read"))
    for name, c in (("claude-code", claude), ("codex", codex)):
        print("    %-12s %5d %8d %6d %5d %5d %6d %11d %10s %11s" % (
            name, c["files_read"], c["sessions"], c["spans_sent"], c["llm_steps"], c["tool_steps"],
            c["agent_spans"], c["tool_errors"], "{:,}".format(c["input_tokens"]),
            "{:,}".format(c["cache_read_tokens"])))
    lines = claude["transcript"]["lines"] + codex["transcript"]["lines"]
    print("    %d transcript lines, every record understood (unknown 0, malformed 0), every count"
          " what was written (asserted)" % lines)
    print("    both imports: %.2f s, sessions read whole, spans sent over HTTP" % float(seconds))
    print("    tokens in: Anthropic counts the uncached remainder, OpenAI includes the cached part —"
          " recorded as each reports")


def reimported(path):
    counts = load(path)
    must(counts["files_unchanged"] == counts["files_seen"] and counts["spans_sent"] == 0,
         "a second import with the same state file should send nothing: %s" % counts)
    print("    again, same state file: %d files unchanged, %d spans sent — safe to re-run (asserted)"
          % (counts["files_unchanged"], counts["spans_sent"]))


# ------------------------------------------------------------------- cost


def rate_for(pricing, model):
    """The pricing file's rule: an exact name, else the longest `prefix*`."""
    models = pricing["models"]
    if model in models:
        return models[model]
    best = None
    for key in models:
        if key.endswith("*") and model.startswith(key[:-1]) and (best is None or len(key) > len(best)):
            best = key
    must(best is not None, "the demo's pricing file has no rate for %s" % model)
    return models[best]


def transcript_costs(doc, pricing):
    """Cost per (session, agent) from the transcripts alone: Anthropic usage
    counted once per message id, under the record's sessionId (a sub-agent's
    records carry its parent session's); Codex usage from each token_count's
    last response, under the rollout's thread id."""
    costs = {}
    claude_root = doc["claude_code"]["path"]
    for folder, _, names in os.walk(claude_root):
        for name in names:
            if not name.endswith(".jsonl"):
                continue
            path = os.path.join(folder, name)
            agent = "claude-code"
            if os.path.basename(folder) == "subagents":
                agent = load(path[:-len(".jsonl")] + ".meta.json")["agentType"]
            seen = set()
            for line in open(path):
                record = json.loads(line)
                message = record.get("message") or {}
                if record.get("type") != "assistant" or message.get("id") in seen:
                    continue
                seen.add(message["id"])
                use, rate = message["usage"], rate_for(pricing, message["model"])
                cost = (use["input_tokens"] * rate["input_per_mtok"]
                        + use["output_tokens"] * rate["output_per_mtok"]
                        + use["cache_read_input_tokens"] * rate["cache_read_per_mtok"]
                        + use["cache_creation_input_tokens"] * rate["cache_write_per_mtok"]) / 1e6
                key = (record["sessionId"], agent)
                costs[key] = costs.get(key, 0.0) + cost
    for path in doc["codex"]["files"]:
        model = thread = None
        for line in open(path):
            record = json.loads(line)
            payload = record["payload"]
            if record["type"] == "session_meta":
                thread = payload["id"]
            if record["type"] == "turn_context":
                model = payload["model"]
            if record["type"] == "event_msg" and payload.get("type") == "token_count" and payload.get("info"):
                use, rate = payload["info"]["last_token_usage"], rate_for(pricing, model)
                cached = use["cached_input_tokens"]
                cost = ((use["input_tokens"] - cached) * rate["input_per_mtok"]
                        + cached * rate.get("cache_read_per_mtok", rate["input_per_mtok"])
                        + use["output_tokens"] * rate["output_per_mtok"]) / 1e6
                costs[(thread, "codex")] = costs.get((thread, "codex"), 0.0) + cost
    return costs


def cost(manifest_path, pricing_path, *topologies):
    doc = manifest(manifest_path)
    pricing = load(pricing_path)
    labels = {v: k for k, v in doc["sessions"].items()}
    agents = {}
    served = {}
    per_session = []
    for path in topologies:
        topo = load(path)
        must(not topo["truncated"] and not topo["nodes_truncated"], "a whole graph: %s" % path)
        total = 0.0
        for node in topo["nodes"]:
            row = agents.setdefault(node["agent"], {"sessions": set(), "llm": 0, "tool": 0, "cost": 0.0})
            row["sessions"].add(topo["run"])
            row["llm"] += node["llm_calls"]
            row["tool"] += node["tool_calls"]
            row["cost"] += node["cost_usd"]
            key = (topo["run"], node["agent"])
            served[key] = served.get(key, 0.0) + node["cost_usd"]
            total += node["cost_usd"]
        per_session.append((topo["run"], total, topo))
    grand = sum(r["cost"] for r in agents.values())
    # Each session's agents, one by one: the per-agent and per-session totals
    # printed below are sums of these.
    recomputed = transcript_costs(doc, pricing)
    must(set(recomputed) == set(served),
         "sessions' agents differ: %s vs %s" % (sorted(recomputed), sorted(served)))
    for (run, agent), value in sorted(served.items()):
        must(abs(value - recomputed[(run, agent)]) < 1e-9,
             "%s in %s %s: the server says %.8f, the transcripts and pricing file say %.8f"
             % (agent, labels.get(run, "?"), short(run), value, recomputed[(run, agent)]))
    print("    %-16s %8s %5s %5s %9s  %s" % ("agent", "sessions", "llm", "tool", "cost", "share"))
    for agent, row in sorted(agents.items(), key=lambda item: -item[1]["cost"]):
        kind = "" if agent in ("claude-code", "codex") else "  (sub-agent)"
        print("    %-16s %8d %5d %5d %9s  %-25s%s" % (tidy(agent), len(row["sessions"]), row["llm"], row["tool"],
                                                    money(row["cost"]), bar(row["cost"] / grand),
                                                    "%4.1f%%%s" % (100 * row["cost"] / grand, kind)))
    print("    %-16s %8s %5s %5s %9s" % ("all", "", "", "", money(grand)))
    print()
    print("    by session:")
    for run, total, topo in per_session:
        parts = " + ".join("%s %s" % (tidy(n["agent"]), money(n["cost_usd"]))
                           for n in sorted(topo["nodes"], key=lambda n: -n["cost_usd"]))
        print("      %-8s %-9s %s%s" % (labels.get(run, "?"), short(run), money(total),
                                       "  = " + parts if len(topo["nodes"]) > 1 else ""))
    print("    all %d per-session agent costs recomputed from the transcripts' own usage and the pricing"
          % len(served))
    print("    file, and every figure above is a sum of them: equal to the cent and beyond (asserted)")


# ----------------------------------------------------------------- autopsy


def duration(ns):
    seconds = ns / 1e9
    return "%dm %02ds" % (seconds // 60, seconds % 60) if seconds >= 60 else "%.1fs" % seconds


def autopsy(manifest_path, cache_path, tape_path):
    manifest(manifest_path)
    doc, tape = load(cache_path), load(tape_path)
    agent_of = {s["index"]: tidy(s["agent"]) for s in tape["steps"]}
    summary = doc["summary"]
    by = summary["by_cause"]
    must(by["ttl_expired"] == 1 and summary["breaks"] == 2 and summary["disagreements"] == 0,
         "expected one TTL break and one compaction: %s" % summary)
    ttl = next(s for s in doc["steps"] if s["cause"] == "ttl_expired")
    must(ttl["gap_ns"] > doc["ttl_seconds"] * 1e9 and ttl["est_cost_lost_usd"] > 0,
         "the TTL break should be priced: %s" % ttl)
    must(ttl["cache_read"] == 0, "the provider reported no cache read after the pause")
    compacted = [s for s in doc["steps"] if (s.get("first_divergent_chunk") or {}).get("change") == "rewritten"]
    must(len(compacted) == 1, "one rewritten context — the /compact: %s" % compacted)
    agents = []
    for step in doc["steps"]:
        if agent_of[step["step_index"]] not in agents:
            agents.append(agent_of[step["step_index"]])
    print("    %d model calls in %d conversations (%s); %d stable — the recorded prefix held"
          % (summary["steps"], len(agents), ", ".join(agents), by["stable"]))
    print()
    print("    %-5s %-14s %-12s %9s %11s %12s  %s" % ("step", "agent", "cause", "gap", "cache read",
                                                    "cache write", "what changed"))
    for step in doc["steps"]:
        if step["cause"] == "stable":
            continue
        change = (step.get("first_divergent_chunk") or {}).get("change")
        if step["cause"] == "ttl_expired":
            what = "TTL passed: ~%d tokens re-processed, %s lost" % (step["est_tokens_lost"],
                                                                     money(step["est_cost_lost_usd"]))
        elif change == "rewritten":
            what = "rewritten — the /compact"
        elif step["previous_step_index"] is None and step["gap_ns"] is None:
            what = "first call of its conversation"
        else:
            what = change or ""
        print("    %-5d %-14s %-12s %9s %11s %12s  %s" % (
            step["step_index"], agent_of[step["step_index"]], step["cause"],
            duration(step["gap_ns"]) if step["gap_ns"] else "—", "{:,}".format(step["cache_read"]),
            "{:,}".format(step["cache_write"]), what))
    print()
    print("    culprits, costliest first (the advice is a lookup table in the server, not generated):")
    for culprit in summary["culprits"]:
        name = culprit["cause"] + (" · " + culprit["change"] if culprit.get("change") else "")
        print("      %-24s %d break  ~%d tokens  %s" % (name, culprit["breaks"], culprit["est_tokens_lost"],
                                                     money(culprit["est_cost_lost_usd"])))
        print("        %s" % tidy(culprit["advice"]))
    print("    total lost: ~%d tokens, %s, priced at the input rate less the cache-read rate"
          % (summary["est_tokens_lost"], money(summary["est_cost_lost_usd"])))
    print("    a transcript records no system prompt or tool definitions, so the autopsy prices the")
    print("    recorded conversation only; the provider's own count for step %d — %s tokens written"
          % (ttl["step_index"], "{:,}".format(ttl["cache_write"])))
    print("    to the cache, none read — covers those too")


# -------------------------------------------------------------------- xray


def provenance(chunk):
    """`step  1 Agent`: the step that introduced a chunk — a tool step by its
    tool, a model call or agent step by its kind."""
    found = chunk.get("introduced_by")
    if not found:
        return "—"
    name = tidy(found.get("name") or "", 9) if found["kind"] == "tool" else found["kind"]
    return "step %2d %s" % (found["step_index"], name)


def is_text(chunk, text):
    """Whether an X-Ray chunk is this text. The server describes a chunk by
    its length and a 120-character preview, so the two are matched on both:
    the same number of characters, and the preview a prefix of the text."""
    preview = chunk["preview"] or ""
    if preview.endswith("…") and chunk["chars"] > len(preview) - 1:
        preview = preview[:-1]
    return chunk["chars"] == len(text) and text.startswith(preview)


def coupons_texts(doc):
    """The coupons session's first prompt and its /compact summary, read whole
    from the transcript the importer read, never from a preview."""
    with open(doc["claude_code"]["files"][doc["sessions"]["coupons"]]) as handle:
        records = [json.loads(line) for line in handle]
    prompts = [r["message"]["content"] for r in records if r.get("type") == "user"
               and isinstance(r["message"]["content"], str) and not r.get("isCompactSummary")]
    summaries = [r["message"]["content"] for r in records if r.get("isCompactSummary")]
    must(prompts and len(summaries) == 1, "the coupons transcript has a first prompt and one /compact summary")
    return prompts[0], summaries[0]


def xray(manifest_path, before_path, after_path):
    doc = manifest(manifest_path)
    before, after = load(before_path), load(after_path)
    first_prompt, compact_summary = coupons_texts(doc)
    totals = before["totals"]
    print("    X-Ray of step %d (%s) — the last call before the /compact:"
          % (before["step_index"], tidy(before["model"])))
    print("    %d chunks, ~%s est. tokens of conversation. The provider's count, %s, also covers"
          % (totals["chunks"], "{:,}".format(totals["est_tokens"]),
             "{:,}".format(totals["reported_context_tokens"])))
    print("    the system prompt and tool definitions, which a transcript does not record.")
    print()
    print("    %3s  %-11s %5s  %-15s %4s  %s" % ("#", "kind", "~tok", "introduced by", "age", "preview"))
    for chunk in before["chunks"]:
        flag = " (repeat)" if chunk.get("duplicate_of") is not None else ""
        print("    %3d  %-11s %5d  %-15s %4s  %s%s" % (
            chunk["index"], chunk["kind"], chunk["est_tokens"], provenance(chunk),
            chunk["age_steps"] if chunk["age_steps"] is not None else "—",
            tidy(chunk["preview"], 52 - len(flag)), flag))
    sources = before["composition"]["by_source"]
    print()
    print("    by source: " + " · ".join("%s %.0f%%" % (tidy(s["key"]), 100 * s["share"]) for s in sources[:6]))
    if totals["duplicate_est_tokens"]:
        print("    repeats: ~%d tokens of content the context already held" % totals["duplicate_est_tokens"])
    # Every chunk is traced to the step that introduced it.
    must(all(c.get("introduced_by") for c in before["chunks"]), "every chunk should have a provenance")
    tools = {c["introduced_by"]["name"] for c in before["chunks"] if c["kind"] == "tool_result"}
    must({"Agent", "Write", "Edit", "Bash"} <= tools, "tool results traced to their tool steps: %s" % tools)

    # The next call, after the compaction.
    must(after["previous"]["step_index"] == before["step_index"], "the X-Ray pairs the call with its predecessor")
    dropped = after["dropped_since_previous"]
    lost = [d for d in dropped if d["kind"] == "instruction" and not d.get("unverified")]
    # The rule is in the first prompt, read whole. The dropped chunk is that
    # prompt: its digest names a chunk of the call before, and that chunk is
    # the prompt's text by length and preview, wherever the rule sits in it.
    must(doc["rule"] in first_prompt, "the first prompt gives the migrations rule")
    by_digest = {c["digest"]: c for c in before["chunks"]}
    rule = [d for d in lost if d["digest"] in by_digest and is_text(by_digest[d["digest"]], first_prompt)]
    must(rule and rule[0]["introduced_by"]["step_index"] == 0,
         "the compaction should have dropped the first prompt, and its rule, given at step 0: %s" % lost)
    # And the summary that replaced the context, read whole: a chunk the
    # server rebuilt is that text, and nowhere in it is migrations/.
    replaced = [c for c in after["chunks"] if is_text(c, compact_summary)]
    must(replaced, "a chunk of the call after the /compact should be the summary the transcript holds"
         " (%d characters): %s" % (len(compact_summary), [(c["chars"], c["preview"]) for c in after["chunks"]]))
    must("migrations" not in compact_summary, "the /compact summary should not mention migrations/")
    print()
    print("    step %d, the first call after the /compact: %d chunks, ~%d est. tokens"
          % (after["step_index"], after["totals"]["chunks"], after["totals"]["est_tokens"]))
    for chunk in after["chunks"]:
        print("    %3d  %-11s %5d  %-15s %4s  %s" % (chunk["index"], chunk["kind"], chunk["est_tokens"],
                                                  provenance(chunk),
                                                  chunk["age_steps"] if chunk["age_steps"] is not None else "—",
                                                  tidy(chunk["preview"], 52)))
    print()
    print("    dropped since step %d: %d chunks, ~%d tokens — %d of them instructions the agent was given:"
          % (before["step_index"], len(dropped), sum(d["est_tokens"] for d in dropped), len(lost)))
    for item in lost:
        print("      step %2d  %s" % (item["introduced_by"]["step_index"], tidy(item["preview"])))
    print("    the summary that replaced them never mentions migrations/: all %d characters, read from"
          % len(compact_summary))
    print("    the transcript and matched to chunk %d by length and preview (asserted)" % replaced[0]["index"])


# --------------------------------------------------------------------- TQL


def findings(manifest_path, findings_path, tape_path):
    doc = manifest(manifest_path)
    labels = {v: k for k, v in doc["sessions"].items()}
    found = load(findings_path)
    tape = load(tape_path)
    by_name = {d["name"]: d for d in found["detectors"]}
    tamper = by_name["test_tampering"]
    must(tamper["runs_matched"] == 1 and tamper["examples"][0]["run"] == doc["sessions"]["tests"],
         "test tampering should be found in the tests session: %s" % tamper)
    unverified = by_name["unverified_finish"]
    must(unverified["runs_matched"] == 2, "two sessions finished without testing: %s" % unverified)
    print("    GET /v1/detectors/findings — %d detectors over the window's %d sessions:"
          % (len(found["detectors"]), found["runs_checked"]))
    print()
    for det in found["detectors"]:
        if not det["runs_matched"]:
            continue
        runs = ", ".join("%s %s" % (labels.get(e["run"], "?"), short(e["run"])) for e in det["examples"])
        print("    %-18s %-6s %d run%s  %s" % (det["name"], det["severity"], det["runs_matched"],
                                               "" if det["runs_matched"] == 1 else "s", runs))
        print("    %-18s %s" % ("", tidy(det["title"])))
    quiet = [d["name"] for d in found["detectors"] if not d["runs_matched"]]
    print("    no match: %s" % ", ".join(quiet))
    example = tamper["examples"][0]
    must(example["outcome"] == "success", "the tampering session reads as a success: %s" % example)
    steps = {s["index"]: s for s in tape["steps"]}
    print()
    print("    test_tampering in the tests session — the steps the pattern matched:")
    for span in example["spans"]:
        step = steps[span["step_index"]]
        args = step.get("request") if isinstance(step.get("request"), dict) else {}
        if "command" in args:
            result = str(step.get("response") or "").strip().splitlines()
            print("      step %d  %-5s %-36s %s: %s" % (
                span["step_index"], tidy(span["name"]), tidy(args["command"], 36),
                "failed" if step["status"] == "error" else "passed", tidy(result[-1] if result else "", 40)))
        else:
            print("      step %d  %-5s %-36s - %s" % (span["step_index"], tidy(span["name"]),
                                                      tidy(args.get("file_path", ""), 36),
                                                      tidy(args.get("old_string", "").strip(), 56)))
            print("      %-50s + %s" % ("", tidy(args.get("new_string", "").strip(), 56)))
    print("    outcome: %s — inferred from the last span, so tampering reads as success; the"
          % example["outcome"])
    print("    detector is how you find it (query: %s)" % tidy(tamper["query"], 60))


def search(manifest_path, search_path, compact_step):
    doc = manifest(manifest_path)
    found = load(search_path)
    must(len(found["matches"]) == 1, "one session wrote under migrations/: %s" % found["matches"])
    match = found["matches"][0]
    must(match["run"] == doc["sessions"]["coupons"], "it should be the coupons session")
    step = match["spans"][0]["step_index"]
    must(step > int(compact_step), "the migration was written after the compaction")
    print("    %s" % found["query"])
    print("    → coupons %s, step %d: %s — %d steps after the /compact at step %s dropped the rule"
          % (short(match["run"]), step, tidy(match["spans"][0]["name"]), step - int(compact_step), compact_step))
    print("    (%d sessions scanned; a search is a regular expression over steps, not a model)"
          % found["runs_scanned"])


# ------------------------------------------------------------------- blame


def blame(manifest_path, index, answer_path, code):
    doc = manifest(manifest_path)
    target = doc["blame"][int(index)]
    labels = {v: k for k, v in doc["sessions"].items()}
    must(code == "0", "blame should find the line (exit 0), exited %s" % code)
    answer = load(answer_path)
    first = answer["matches"][0]
    must(first["run"] == target["session"] and first["tool"] == target["tool"],
         "expected %s by %s, got %s" % (target["session"], target["tool"], first))
    when = time.strftime("%H:%M:%SZ", time.gmtime(first["at_ns"] / 1e9))
    print("    $ traza-server blame %s:%d" % (target["file"], target["line"]))
    print("      %s" % tidy(answer["line"]))
    print("      ← %s, step %d of %s %s (%s, %s) at %s" % (
        tidy(first["tool"]), first["step_index"], labels.get(first["run"], "?"), short(first["run"]),
        tidy(first["service"]), tidy(first["model"]), when))
    print("        prompt: \"%s\"" % tidy(first.get("prompt", "")))


# ------------------------------------------------------------------- watch


class RedirectRefused(Exception):
    """A 3xx answered a request that carried the token."""


class RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect: urllib would copy Authorization onto it."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RedirectRefused("%s answered %d, a redirect to %s: not followed, so the token goes only "
                              "to the URL it was given" % (req.full_url, code, newurl))


OPENER = urllib.request.build_opener(RefuseRedirects)


def request(url, token, path):
    req = urllib.request.Request(url + path)
    req.add_header("Authorization", "Bearer " + token)
    with OPENER.open(req, timeout=10) as reply:
        return json.loads(reply.read().decode())


def arrived(url, token, run, steps, wrote, out):
    """Polls the run's tape until it holds `steps` steps; prints how long the
    appended line took to become part of the run."""
    wrote = float(wrote)
    deadline = wrote + 15
    while True:
        tape = request(url, token, "/v1/tapes/" + run)
        if len(tape["steps"]) >= int(steps):
            took = time.time() - wrote
            break
        must(time.time() < deadline, "the watch did not deliver the appended lines within 15 s")
        time.sleep(0.02)
    with open(out, "w") as handle:
        json.dump(tape, handle)
    last = tape["steps"][-1]
    print("    on the tape %.2f s after the write: step %d, %s %s" % (
        took, last["index"], last["kind"], tidy(last.get("tool") or last.get("model"))))
    must(took < 10, "an appended line should arrive within seconds")


def live(url, token, run, tool):
    """Waits for the run to show as live, its current step the tool."""
    deadline = time.time() + 10
    while True:
        rows = [r for r in request(url, token, "/v1/runs/live")["runs"] if r["run"] == run]
        if rows and (rows[0].get("current") or {}).get("name") == tool:
            break
        must(time.time() < deadline, "the session never showed as live running %s: %s" % (tool, rows))
        time.sleep(0.05)
    row = rows[0]
    print("    GET /v1/runs/live → %s live: %d steps, now running %s %s, verdict %s" % (
        short(run), row["steps"], row["current"]["kind"], tidy(row["current"]["name"]), row["verdict"]))


def compacted(cache_path):
    """The rewritten (compacted) call the autopsy found, and the call before
    it, as words for the shell: step, previous step, trace, span, previous
    span."""
    doc = load(cache_path)
    steps = {s["step_index"]: s for s in doc["steps"]}
    rewritten = [s for s in doc["steps"]
                 if (s.get("first_divergent_chunk") or {}).get("change") == "rewritten"]
    must(len(rewritten) == 1, "the autopsy should find one rewritten (compacted) call, found %d" % len(rewritten))
    step = rewritten[0]
    must(step["previous_step_index"] in steps,
         "the rewritten call's predecessor should be in the autopsy: %s" % step["previous_step_index"])
    print(step["step_index"], step["previous_step_index"], step["span"]["trace_id"], step["span"]["span_id"],
          steps[step["previous_step_index"]]["span"]["span_id"])


def rechecked(manifest_path, before_path, after_path):
    doc = manifest(manifest_path)
    before = next(d for d in load(before_path)["detectors"] if d["name"] == "unverified_finish")
    after = next(d for d in load(after_path)["detectors"] if d["name"] == "unverified_finish")
    runs_after = [e["run"] for e in after["examples"]]
    must(after["runs_matched"] == before["runs_matched"] - 1 and doc["sessions"]["rename"] not in runs_after,
         "the rename session ran its tests and should no longer match: %s" % after)
    print("    unverified_finish: %d sessions before the watch, %d after — the rename session ran"
          " its tests (asserted)" % (before["runs_matched"], after["runs_matched"]))


MODES = {
    "imported": imported,
    "reimported": reimported,
    "cost": cost,
    "autopsy": autopsy,
    "xray": xray,
    "findings": findings,
    "search": search,
    "blame": blame,
    "rechecked": rechecked,
    "compacted-step": compacted,
}
LIVE_MODES = {"arrived": arrived, "live": live}

if __name__ == "__main__":
    mode = sys.argv[1]
    if mode in LIVE_MODES:
        try:
            LIVE_MODES[mode](*sys.argv[2:])
        except (RedirectRefused, urllib.error.URLError, OSError) as error:
            print("render: %s" % error, file=sys.stderr)
            sys.exit(1)
    else:
        MODES[mode](*sys.argv[2:])
