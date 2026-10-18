#!/usr/bin/env python3
"""Renders and asserts one step of the foresight demo.

Kept out of the shell script for the reason the other demos keep theirs:
quoting JSON inside `python3 -c` is where scripts like this go wrong. Every
mode prints what the server answered and exits non-zero when it is not what
the demo claims. The statistics the demo leans on — every Wilson interval it
prints, the Fisher exact test behind a fatal fork, the Brier skill — are
recomputed here with the standard library from the counts the server gave,
and checked against the server's own figures, so the demo does not take the
server's arithmetic on trust.
"""

import json
import math
import sys

# The behaviour corpus's ground truth (src/seed.rs, BRANCHES): the
# support-agent's branches as (runs, successes, abandoned), at scale 1.
SUPPORT_BRANCHES = {
    "clean refund": (60, 54, 0),
    "search loop": (50, 5, 5),
    "escalation": (40, 24, 4),
    "double refund": (30, 15, 0),
}
LIVE_SUPPORT_RUNS = 6
Z = 1.959963984540054
# The tree's label column; a longer path prints on a line of its own.
WIDTH = 38


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def lines(path):
    with open(path) as handle:
        return [json.loads(line) for line in handle if line.strip()]


def safe(text):
    """Text that came from telemetry — tool names, run ids, explanations that
    quote them — printed with control characters replaced, so a recorded name
    cannot drive the terminal."""
    return "".join(c if c.isprintable() else " " for c in str(text))


def pct(value):
    return "%.1f%%" % (100 * value)


def wilson(successes, n):
    """The 95% Wilson score interval, as the foresight guide defines it."""
    p = successes / n
    z2 = Z * Z
    centre = (p + z2 / (2 * n)) / (1 + z2 / n)
    half = Z * math.sqrt(p * (1 - p) / n + z2 / (4 * n * n)) / (1 + z2 / n)
    return centre - half, centre + half


def ci(successes, n, served):
    """The interval recomputed from the counts, checked against the one the
    server answered (rounded to four places there) — printed from the
    recomputed value, so it reads the same as the server's explanations."""
    lo, hi = wilson(successes, n)
    must(abs(lo - served[0]) < 6e-5 and abs(hi - served[1]) < 6e-5,
         "the server's interval %s for %d of %d; recomputed %.5f–%.5f" % (served, successes, n, lo, hi))
    return lo, hi


def interval(bounds):
    """An interval for a column: padded to line up."""
    return "%4.1f–%4.1f%%" % (100 * bounds[0], 100 * bounds[1])


def prose(bounds):
    """An interval for a sentence."""
    return "%.1f–%.1f%%" % (100 * bounds[0], 100 * bounds[1])


def fisher(a, b, c, d):
    """Two-sided Fisher exact test of [[a, b], [c, d]]: the probability,
    under no association, of a table at most as likely as this one."""
    row, col, n = a + b, a + c, a + b + c + d
    total = math.comb(n, col)

    def prob(x):
        return math.comb(row, x) * math.comb(n - row, col - x) / total

    observed = prob(a)
    low, high = max(0, col - (n - row)), min(row, col)
    return sum(p for p in (prob(x) for x in range(low, high + 1)) if p <= observed * (1 + 1e-7))


def whisker(p, bounds, base, width=20):
    """A 95% interval on a 0–100% axis: ├─●─┤, and ┊ the agent's base rate."""
    cells = [" "] * (width + 1)
    lo, hi, at, mark = (round(v * width) for v in (bounds[0], bounds[1], p, base))
    if mark not in range(lo, hi + 1):
        cells[mark] = "┊"
    for i in range(lo, hi + 1):
        cells[i] = "─"
    cells[lo], cells[hi] = "├", "┤"
    cells[at] = "●"
    return "▕" + "".join(cells) + "▏"


def axis(width=20):
    return " " + "0%".ljust(width // 2) + "50%".ljust(width - width // 2) + "100%"


def odds(successes, n, p, served, base):
    """`5 of 50   10.0%   4.3–21.4%  ▕├●─┤ ┊ ▏` — counts, rate, interval, bar."""
    bounds = ci(successes, n, served)
    return "%3d of %-3d %6s %11s %s" % (successes, n, pct(p), interval(bounds), whisker(p, bounds, base))


# ------------------------------------------------------------------ modes


def seeded(log_path):
    text = open(log_path).read().strip()
    must("behaviour runs" in text, "seed did not report a behaviour corpus: %r" % text)
    words = text.split()
    spans, runs = int(words[words.index("wrote") + 1]), int(words[words.index("of") + 1])
    print("    seed --behaviours wrote %d spans of %d runs into the data directory, before the"
          " server opened it" % (spans, runs))


def cohorts(path):
    doc = load(path)
    runs = sum(r for r, _, _ in SUPPORT_BRANCHES.values())
    successes = sum(s for _, s, _ in SUPPORT_BRANCHES.values())
    abandoned = sum(a for _, _, a in SUPPORT_BRANCHES.values())
    support = [c for c in doc["cohorts"] if c["agent"] == "support-agent" and "task" not in c]
    must(len(support) == 1, "no support-agent cohort: %s" % doc["cohorts"])
    cohort = support[0]
    must((cohort["runs"], cohort["success"], cohort["abandoned"], cohort["unknown"])
         == (runs, successes, abandoned, LIVE_SUPPORT_RUNS),
         "the support-agent cohort disagrees with the corpus's ground truth: %s" % cohort)
    print()
    print("    %-28s %5s %8s %8s %10s %8s %10s" % ("cohort", "runs", "success", "failure",
                                                  "abandoned", "running", "base rate"))
    for c in doc["cohorts"]:
        name = safe(c["agent"]) + (", task %s…" % safe(c["task"][:6]) if c.get("task") else "")
        print("    %-28s %5d %8d %8d %10d %8d %10s" % (name, c["runs"], c["success"], c["failure"],
                                                      c["abandoned"], c["unknown"], pct(c["base_rate"])))
    print()
    print("    runs counts completed runs; the %d still running have no outcome yet and sit in"
          " no rate" % LIVE_SUPPORT_RUNS)
    print("    support-agent matches the corpus's ground truth: %d runs, %d succeeded, %d abandoned"
          " (asserted)" % (runs, successes, abandoned))


def ground(*branches):
    """(successes, runs) over some of the corpus's support branches."""
    return (sum(SUPPORT_BRANCHES[b][1] for b in branches), sum(SUPPORT_BRANCHES[b][0] for b in branches))


def short(token, model):
    return "llm" if token == "llm:" + model else safe(token)


def fate_map(path):
    doc = load(path)
    must(doc["agent"] == "support-agent", "map of %r" % doc["agent"])
    must(not doc["truncated"] and doc["nodes_shown"] == doc["nodes_total"],
         "the corpus's map should be whole: %s" % doc["bounds"])
    base = doc["base_rate"]
    root = doc["root"]
    first = root["children"]
    must(len(first) == 1 and first[0]["token"].startswith("llm:"),
         "every support-agent run starts with one model call: %s" % [c["token"] for c in first])
    model = first[0]["token"][4:]
    print("    support-agent · %d completed runs · base rate %s · llm = llm:%s, its one model"
          % (doc["runs"], pct(base), safe(model)))
    print()
    print("    %-*s %5s %8s %12s %s" % (WIDTH, "path", "runs", "success", "95% CI", axis()))

    def chain(node):
        """A run of single children with the same visits reads as one line."""
        tokens = [node["token"]]
        while (len(node.get("children", [])) == 1 and not node.get("omitted_children")
               and node["children"][0]["visits"] == node["visits"]):
            node = node["children"][0]
            tokens.append(node["token"])
        return tokens, node

    def show(node, prefix, connector):
        tokens, last = chain(node)
        label = prefix + connector + " › ".join(short(t, model) for t in tokens)
        inner = prefix + {"└─ ": "   ", "├─ ": "│  "}.get(connector, "")
        children = last.get("children", [])
        note = inner + ("│ " if children else "  ")
        if len(label) > WIDTH:
            print("    " + label)
            label = note
        bounds = ci(node["success"], node["visits"], node["ci95"])
        print("    %-*s %5d %8s %12s %s" % (WIDTH, label, node["visits"], pct(node["success_rate"]),
                                           interval(bounds), whisker(node["success_rate"], bounds, base)))
        if node.get("hotspot"):
            print("    %s hotspot: %.2f bits of uncertainty in what runs did next"
                  % (note, node["entropy"]))
        if node.get("fatal_fork"):
            fork = node["fork"]
            print("    %s FATAL FORK: %.1f points below its siblings (%s of %d), Fisher p = %.1e"
                  % (note, 100 * fork["gap"], pct(fork["siblings_success_rate"]),
                     fork["siblings_visits"], fork["p_value"]))
        for n, child in enumerate(children):
            show(child, inner, "└─ " if n == len(children) - 1 else "├─ ")

    show(first[0], "", "")
    print("    (bars: the 95% Wilson interval and its estimate ●, on 0–100%; ┊ the base rate)")

    forks = doc["fatal_forks"]
    must([f["path"][-1] for f in forks] == ["tool:search_orders×3-5", "tool:issue_refund×2"],
         "the corpus has two fatal forks, the loop first: %s" % [f["path"] for f in forks])
    top = forks[0]
    node = root
    for tok in top["path"]:
        node = next(c for c in node["children"] if c["token"] == tok)
    successes, visits = node["success"], node["visits"]
    must((successes, visits) == ground("search loop"), "the loop's counts: %d of %d" % (successes, visits))
    bounds = ci(successes, visits, node["ci95"])
    sib_n = top["fork"]["siblings_visits"]
    sib_s = round(top["fork"]["siblings_success_rate"] * sib_n)
    p = fisher(successes, visits - successes, sib_s, sib_n - sib_s)
    must(abs(math.log10(p) - math.log10(top["fork"]["p_value"])) < 0.01,
         "Fisher p %.3e, recomputed %.3e" % (top["fork"]["p_value"], p))
    examples = {e["outcome"]: e["run"] for e in node["example_runs"]}
    must("success" in examples and "failure" in examples, "a fork carries a pair to diff: %s" % examples)
    print()
    print("    the fatal fork: %s" % " › ".join(safe(t) for t in top["path"]))
    print("      %d of %d runs through it succeeded (%s, 95%% CI %s) against %d of %d for its siblings"
          % (successes, visits, pct(node["success_rate"]), prose(bounds), sib_s, sib_n))
    print("      recomputed here from those counts: Wilson %s, Fisher exact p = %.1e (asserted)"
          % (prose(bounds), p))
    print("      a success and a failure from the same point, ready for GET /v1/diff: %s · %s"
          % (safe(examples["success"]), safe(examples["failure"])))
    print("    %d nodes from %d spans; nothing was pruned, capped or dropped (bounds asserted)"
          % (doc["nodes_total"], doc["bounds"]["spans_read"]))


def doomed(steps_path, run):
    steps = lines(steps_path)
    must(len(steps) == 3, "the unguarded agent should have taken three steps, took %d" % len(steps))
    print("    %-4s %-20s %-9s %-15s %s" % ("step", "taken", "check", "foresight",
                                           "how earlier runs on this path ended"))
    for step in steps:
        must(step["check"]["verdict"] == "continue", "no policy acts yet: %s" % step["check"])
        must("foresight" not in step["check"], "no policy sets min_success_odds yet")
        fate = step["fate"]
        node = fate["node"]
        stats = odds(node["success"], fate["support"], fate["p_success"], fate["ci95"], fate["base_rate"])
        empty = "  empty result" if step["kind"] == "tool" and step["status"] == "error" else ""
        print("    %-4d %-20s %-9s %-15s %s%s" % (step["step"], "%s:%s" % (step["kind"], step["name"]),
                                                "continue", fate["verdict"], stats, empty))
    verdicts = [s["fate"]["verdict"] for s in steps]
    must(verdicts == ["on_track", "on_track", "likely_failure"],
         "expected on_track, on_track, likely_failure: %s" % verdicts)
    # The ground truth: every run starts with the model; one search is shared
    # by the refunds and the loop; a second search only by the loop.
    expected = [ground("clean refund", "search loop", "escalation", "double refund"),
                ground("clean refund", "search loop", "double refund"), ground("search loop")]
    got = [(s["fate"]["node"]["success"], s["fate"]["support"]) for s in steps]
    must(got == expected, "foresight's counts %s, the corpus says %s" % (got, expected))
    last = steps[-1]["fate"]
    must(last["last_open"] and last["outcome"] == "unknown", "the run is live: %s" % last)
    must(last["ci95"][1] < 0.5, "likely_failure means the upper bound is below 50%")
    print()
    print("    GET /v1/fate/runs/%s after step 3 →" % run)
    print("      path      %s" % "  ".join(safe(t) for t in last["path"]))
    print("      compared  %s — its last step may still repeat, so it is pooled with every"
          % ", ".join(safe(t) for t in last["node"]["pooled"]))
    print("                run that repeated the search at least as often")
    print("      verdict   %s: %s" % (last["verdict"], safe(last["explanation"])))
    print("    the run has not failed — it has no outcome yet; foresight is about what comes next")


def stopped(check_path, steps_path, map_path, run):
    check = load(check_path)
    steps = lines(steps_path)
    built = load(map_path)["built_at_ns"]
    must(len(steps) == 3, "a stopped agent must not have recorded a fourth step: %d" % len(steps))
    must(check["verdict"] == "stop", "the pre-call check should stop the run: %s" % check["verdict"])
    must(check["tripped_by"] is None, "a foresight stop is a prediction, never latched: %s" % check)
    reasons = check["reasons"]
    must(reasons and reasons[0].startswith("foresight: "), "expected a foresight reason: %s" % reasons)
    sight = check["foresight"]
    must(sight["raised"] and sight["next_tool"] == "search_orders" and sight["support"] >= 30,
         "the foresight should raise the action, judged with the next search: %s" % sight)
    limit = check["limits"]["min_success_odds"]
    bounds = ci(sight["successes"], sight["support"], sight["ci95"])
    must(bounds[1] < limit, "the upper bound must be below min_success_odds")
    must(sight["map_built_at_ns"] == built,
         "the fuse should walk the map beat 2 built and cached: %s, not %s" % (sight["map_built_at_ns"], built))
    print("    step 4 would be tool:search_orders. The agent asks first:")
    print()
    print("    POST /v1/fuses/check {\"run\": \"%s\", \"next\": {\"tool\": \"search_orders\"}}" % run)
    print("      verdict   STOP    policy %s · min_success_odds %s" % (check["policy"], limit))
    print("      reason    %s" % safe(reasons[0]))
    needed = check["limits"].get("foresight_min_support", 30)
    print("      why       upper bound %s < %s, on %d earlier runs (%d needed%s)"
          % (pct(bounds[1]), pct(limit), sight["support"], needed,
             "" if "foresight_min_support" in check["limits"] else ", the default"))
    print("      map       the one beat 2 built and cached: same map_built_at_ns (asserted)")
    print("      latched   no (tripped_by null): a prediction, made again at every check — it lifts")
    print("                by itself if the run's path moves on to better odds")
    print()
    print("    the agent halted: 3 steps recorded before the check, 3 after — the fourth search"
          " never ran (asserted)")


def healthy(steps_path, fate_path, run):
    steps = lines(steps_path)
    must(len(steps) == 5, "the healthy agent should take all five steps: %d" % len(steps))
    print("    %-4s %-20s %-9s %-15s %s" % ("step", "about to take", "check", "foresight",
                                           "runs on its path, the step included"))
    judged = []
    refund_step = None
    for step in steps:
        check = step["check"]
        must(check["verdict"] == "continue", "a healthy run must be allowed: %s" % check)
        sight = check.get("foresight") or {}
        label = "%s:%s" % (step["kind"], step["name"])
        if sight.get("support"):
            must(not sight["raised"], "foresight must not act on a healthy run: %s" % sight)
            judged.append(sight)
            if sight.get("next_tool") == "issue_refund":
                refund_step = step["step"]
            stats = odds(sight["successes"], sight["support"], sight["p_success"], sight["ci95"],
                         sight["base_rate"])
            print("    %-4d %-20s %-9s %-15s %s" % (step["step"], label, "continue", sight["verdict"], stats))
        else:
            print("    %-4d %-20s %-9s %-15s %s" % (step["step"], label, "continue", "—",
                                                   "no steps recorded yet: nothing to judge"))
    must(len(judged) >= 3, "foresight should have judged most of the healthy run's steps: %d" % len(judged))
    ahead = [s for s in judged if s.get("next_tool")]
    refund = [s for s in ahead if s["next_tool"] == "issue_refund"]
    must(refund and (refund[0]["successes"], refund[0]["support"]) == ground("clean refund", "double refund"),
         "the check before the refund pools one refund and two: %s" % refund)
    print()
    print("    step %d's check, judged with the refund it was about to issue:" % refund_step)
    print("      %s" % safe(refund[0]["explanation"]))
    fate = load(fate_path)
    must(fate["outcome"] == "success" and not fate["last_open"],
         "the finished run should read as a completed success: %s" % fate)
    must((fate["node"]["success"], fate["support"]) == ground("clean refund"),
         "the finished run is judged at the clean refund's node: %s" % fate)
    print()
    print("    GET /v1/fate/runs/%s, now that it has ended →" % run)
    print("      outcome   %s — a finished run is judged at its own node, its last step no longer open"
          % fate["outcome"])
    print("      verdict   %s: %s" % (fate["verdict"], safe(fate["explanation"])))
    print("    every check answered continue; every count above is the corpus's ground truth (asserted)")


def calibration(path):
    doc = load(path)
    cal = doc["calibration"]
    print("    %-6s %5s %8s %9s %7s" % ("depth", "runs", "Brier", "baseline", "skill"))
    for depth in cal["depths"]:
        skill = 1 - depth["brier"] / depth["baseline_brier"] if depth["baseline_brier"] else 0.0
        must(abs(skill - depth["skill"]) < 1e-3, "skill %s, recomputed %.4f" % (depth["skill"], skill))
        note = "  every run's first step is the same: nothing to tell yet" if depth["depth"] == 1 else ""
        print("    %-6d %5d %8.4f %9.4f %6.1f%%%s" % (depth["depth"], depth["runs"], depth["brier"],
                                                    depth["baseline_brier"], 100 * depth["skill"], note))
    by_depth = {d["depth"]: d for d in cal["depths"]}
    must(by_depth[1]["skill"] == 0.0, "depth 1 cannot beat the base rate here")
    must(by_depth[3]["skill"] > 0.2, "by depth 3 the map should beat the base rate: %s" % by_depth[3])
    print("    skill = 1 − Brier / baseline: the share of the base rate's error the map removes"
          " (recomputed, asserted)")
    print()
    print("    reliability at depth 3 — when the map said p, how often those runs succeeded:")
    for b in cal["reliability"]:
        said, saw = round(20 * b["predicted"]), round(20 * b["observed"])
        bar = "".join("●" if i == saw else ("·" if i == said else " ") for i in range(21))
        print("      said %5s   saw %5s   %3d runs   ▕%s▏" % (pct(b["predicted"]), pct(b["observed"]),
                                                            b["runs"], bar))
    print("    (● what happened, · what was said, on 0–100%; one ● where they agree)")
    print("    each run was predicted from the half of the cohort it is not in (two-fold, split by run id)")


MODES = {
    "seeded": seeded,
    "cohorts": cohorts,
    "map": fate_map,
    "doomed": doomed,
    "stopped": stopped,
    "healthy": healthy,
    "calibration": calibration,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](*sys.argv[2:])
