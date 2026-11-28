"""The ``traza`` command: tape tests, golden runs, forks and a TLS forwarder from a shell.

::

    traza test -- python agent.py --task "{input}"     # every golden run, JUnit XML on stdout
    traza test --junit report.xml --run dr-0917 -- python agent.py
    traza golden add dr-0917                            # make a run a regression test
    traza golden list
    traza forks dr-0917                                 # its branches and where they parted
    traza fork dr-0917 --at 10 --tool-result '{"error": "fetch failed: 404"}' -- python research.py
    traza forward --upstream https://api.trazadb.com    # plain HTTP on 127.0.0.1:8089, TLS upstream

The server is ``--url`` (default ``TRAZA_URL``, else ``http://127.0.0.1:8080``)
and the tenant ``--tenant`` (default ``TRAZA_TENANT``). The credential is
``TRAZA_TOKEN`` only: a token on a command line lands in process listings and
CI logs. The agent ``traza test`` runs is handed ``TRAZA_REPLAY_KEY`` as its
provider API key when set — a ``ro`` credential of the runs' tenant — else
``TRAZA_TOKEN``, which is refused when it is an unbound (operator) credential.

``traza forward`` takes no ``--url``: it forwards whatever local tools send
it to ``--upstream`` (see :mod:`traza.forward`).

Exit status: ``0`` when every test passed (or the command succeeded), ``1``
when a test failed, ``2`` for a usage error or a server that could not be
asked.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from typing import Any, List, Optional, Sequence

from . import __version__, forward as _forward
from .client import Client
from .errors import TrazaError
from .testing import GoldenResult, junit_xml, replay_key, run_golden

#: Golden runs ``traza test`` lists by default: the server's maximum, so a
#: suite is never quietly cut to the server's default page.
GOLDEN_LIMIT = 1000

#: Forks one ``GET /v1/forks`` lists when the caller names no limit.
FORK_LIMIT = 20


def _client(args: argparse.Namespace) -> Client:
    url = args.url or os.environ.get("TRAZA_URL") or "http://127.0.0.1:8080"
    tenant = args.tenant if args.tenant is not None else (os.environ.get("TRAZA_TENANT") or None)
    return Client(url, token=os.environ.get("TRAZA_TOKEN") or None, tenant=tenant, timeout=args.http_timeout)


def _command(args: argparse.Namespace, parser: argparse.ArgumentParser) -> List[str]:
    command = list(args.command or [])
    if not command:
        parser.error("give the agent command after --, for example: traza test -- python agent.py")
    return command


def _say(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def _cmd_test(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    command = _command(args, parser)
    client = _client(args)
    # Settled before any agent runs: an operator token is not handed out.
    key = replay_key(client)
    partial = False
    if args.run:
        targets = [(run, client.tenant) for run in args.run]
    else:
        goldens = client.goldens(limit=args.limit)
        targets = [(golden.run or "", golden.tenant) for golden in goldens.runs if golden.run]
        if goldens.truncated:
            partial = not args.allow_partial
            if len(goldens.runs) >= min(args.limit, GOLDEN_LIMIT) and args.limit < GOLDEN_LIMIT:
                why = f"more golden runs exist than --limit {args.limit}; raise --limit (at most {GOLDEN_LIMIT})"
            elif len(goldens.runs) >= GOLDEN_LIMIT:
                why = (f"more golden runs exist than one listing returns ({GOLDEN_LIMIT}); partition the "
                       "suite with --tenant or --run")
            else:
                why = ("more golden markers exist than one listing examines; partition the suite with "
                       "--tenant or --run")
            _say(f"traza test: the golden listing is incomplete — {why}"
                 + ("" if partial else "; testing the runs listed (--allow-partial)"))
    if not targets:
        if args.allow_empty:
            _say("traza test: no golden runs to test")
        else:
            _say(
                "traza test: no golden runs to test — mark one with `traza golden add RUN` or record "
                "traza.golden=true on its spans — and refusing to report an empty suite as a pass; "
                "pass --allow-empty to accept one"
            )
            return 1
    results: List[GoldenResult] = []
    for run, tenant in targets:
        result = run_golden(client, run, command, tenant=tenant, mode=args.mode, timeout=args.timeout,
                            api_key=key)
        results.append(result)
        verdict = "PASS" if result.passed else ("ERROR" if result.kind == "error" else "FAIL")
        where = f" [{tenant}]" if tenant else ""
        _say(f"{verdict} {run}{where} ({result.seconds:.2f} s)")
        if result.message:
            for line in result.message.splitlines():
                _say("    " + line)
    report = junit_xml(results)
    if args.junit:
        with open(args.junit, "w", encoding="utf-8") as handle:
            handle.write(report)
    else:
        sys.stdout.write(report)
    failed = sum(1 for result in results if not result.passed)
    _say(f"traza test: {len(results) - failed} passed, {failed} failed")
    if partial:
        _say("traza test: failing the suite because it is incomplete; pass --allow-partial to accept that")
    return 1 if failed or partial else 0


def _cmd_golden(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    client = _client(args)
    if args.action == "list":
        goldens = client.goldens(limit=args.limit)
        for golden in goldens.runs:
            where = f"\t{golden.tenant}" if golden.tenant else ""
            print(f"{golden.run}\t{golden.source}{where}")
        if goldens.truncated:
            _say("traza golden: more golden runs exist than listed; raise --limit")
        return 0
    if not args.run:
        parser.error(f"traza golden {args.action} needs a run id")
    client.mark_golden(args.run, golden=args.action == "add")
    _say(f"{args.run} {'marked' if args.action == 'add' else 'no longer marked'} golden")
    return 0


def _cmd_forks(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    forks = _client(args).forks(args.run, limit=args.limit)
    if args.json:
        print(json.dumps(forks.raw, indent=2, sort_keys=True))
        return 0
    if forks.parent is not None:
        parent = forks.parent
        print(f"{forks.run}: {parent.steps} steps, {parent.outcome}, ${parent.cost_usd or 0:.4f}")
    for fork in forks.forks:
        cut = f"cut at step {fork.fork_step}" if fork.fork_step is not None else "cut ?"
        if not fork.diffed:
            print(f"  {fork.run}  {fork.fork_id or '-'}  {cut}  not compared ({fork.skipped})")
            continue
        first = fork.first_divergence
        parted = "matches its parent" if first is None else f"parted at step {first.b} ({first.kind})"
        print(f"  {fork.run}  {fork.fork_id or '-'}  {cut}  {fork.outcome}  ${fork.cost_usd or 0:.4f}  {parted}")
        if first is not None and first.explanation:
            print(f"      {first.explanation}")
    if forks.merged_into_parent:
        _say(f"traza forks: {forks.merged_into_parent} tagged spans joined the parent run itself "
             "(a fork without its own session)")
    if forks.truncated:
        limit = args.limit or FORK_LIMIT
        if any(fork.skipped == "deadline" for fork in forks.forks):
            why = "the query budget ran out before every fork was compared; ask for a smaller --limit"
        elif len(forks.forks) >= limit:
            why = f"more forks exist than --limit {limit}; raise --limit (at most 50)"
        else:
            why = "more tagged spans exist than one listing examines, so the newest forks may be missing"
        _say(f"traza forks: the listing is incomplete — {why}")
    return 0


def _cmd_fork(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    command = _command(args, parser)
    edits = [args.response is not None, args.response_json is not None, args.tool_result is not None]
    if sum(edits) > 1:
        parser.error("give at most one of --response, --response-json, --tool-result")
    options: dict = {}
    if args.response is not None:
        options["response"] = args.response
    if args.response_json is not None:
        options["response"] = json.loads(args.response_json)
    if args.tool_result is not None:
        options["tool_result"] = json.loads(args.tool_result)
    client = _client(args)
    fork = client.fork(args.run, args.at, live=not args.no_live, set_env=False, **options)
    with fork:
        env = dict(os.environ)
        env.update(fork.env())
        # The harness asks the server for its tool results, the edit among
        # them: tell it where the server is, as well as the switch.
        env["TRAZA_URL"] = client.url
        if client.tenant is not None:
            env["TRAZA_TENANT"] = client.tenant
        _say(f"fork {fork.id} of {args.run}, cut at step {fork.step}; its run records as {fork.run_id}")
        code = subprocess.call(command, env=env)
    status = fork.final_status
    if status is not None:
        live = status.fork.live_calls if status.fork is not None else 0
        _say(f"served {status.served} recorded model call(s); {live} call(s) went live")
    return code


def build_parser() -> argparse.ArgumentParser:
    """The ``traza`` argument parser."""
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--url", help="the Traza server (default: $TRAZA_URL, else http://127.0.0.1:8080)")
    common.add_argument("--tenant", help="tenant scope (default: $TRAZA_TENANT)")
    common.add_argument("--http-timeout", type=float, default=30.0, help=argparse.SUPPRESS)

    parser = argparse.ArgumentParser(prog="traza", description="Tape tests, golden runs, forks and a TLS forwarder for Traza.")
    parser.add_argument("--version", action="version", version=f"traza {__version__}")
    commands = parser.add_subparsers(dest="name", metavar="COMMAND")
    commands.required = True

    test = commands.add_parser("test", parents=[common], help="replay golden runs under an agent command",
                               description="Replay every golden run (or each --run) strictly under the agent "
                               "command and report JUnit XML. {run}, {input} and {replay} in the command are "
                               "replaced per run; TRAZA_RUN_INPUT holds the run's first user message.")
    test.add_argument("--run", action="append", help="test this run instead of the golden runs (repeatable)")
    test.add_argument("--mode", choices=["strict", "loose"], default="strict")
    test.add_argument("--timeout", type=float, default=300.0, help="seconds per run (default 300)")
    test.add_argument("--junit", help="write the JUnit XML report here instead of stdout")
    test.add_argument("--limit", type=int, default=GOLDEN_LIMIT,
                      help=f"golden runs to list at most (default {GOLDEN_LIMIT}, the server's maximum)")
    test.add_argument("--allow-empty", action="store_true", help="exit 0 when there is nothing to test")
    test.add_argument("--allow-partial", action="store_true",
                      help="test the golden runs listed when the listing is incomplete, instead of failing")

    golden = commands.add_parser("golden", parents=[common], help="list, add or remove golden runs")
    golden.add_argument("action", choices=["list", "add", "remove"])
    golden.add_argument("run", nargs="?")
    golden.add_argument("--limit", type=int, default=None)

    forks = commands.add_parser("forks", parents=[common], help="a run's forks and where they parted")
    forks.add_argument("run")
    forks.add_argument("--limit", type=int, default=None)
    forks.add_argument("--json", action="store_true", help="print the server's answer as JSON")

    fork = commands.add_parser("fork", parents=[common], help="run an agent command on a fork of a run",
                               description="Replay RUN up to step --at, serve the edit there (if any), and "
                               "send every later model call to the real provider, while the agent command runs.")
    fork.add_argument("run")
    fork.add_argument("--at", type=int, required=True, help="the tape step to cut at")
    fork.add_argument("--response", help="answer text served at the step")
    fork.add_argument("--response-json", help="an assistant message, as JSON, served at the step")
    fork.add_argument("--tool-result", help="a tool result, as JSON, served at the step by "
                      "POST $TRAZA_URL/v1/replays/$TRAZA_REPLAY_ID/tools/next, which the agent's harness asks")
    fork.add_argument("--no-live", action="store_true", help="after the edit, keep serving the recording")

    _forward.add_arguments(commands.add_parser(
        "forward", help="forward plain HTTP on loopback to an https:// Traza server",
        description="Listen on a loopback address and forward each request to the https:// --upstream, "
        "TLS verified: for tools that speak plain HTTP only, such as traza-server import --watch."))
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Runs the ``traza`` command; returns its exit status."""
    parser = build_parser()
    arguments = list(sys.argv[1:] if argv is None else argv)
    # The agent command is everything after the first "--", split off before
    # parsing so that none of its own options can be read as traza's.
    command: List[str] = []
    if "--" in arguments:
        cut = arguments.index("--")
        arguments, command = arguments[:cut], arguments[cut + 1:]
    args = parser.parse_args(arguments)
    args.command = command
    handlers: Any = {"test": _cmd_test, "golden": _cmd_golden, "forks": _cmd_forks, "fork": _cmd_fork,
                     "forward": lambda args, _parser: _forward.run(args)}
    try:
        return int(handlers[args.name](args, parser))
    except TrazaError as error:
        _say(f"traza {args.name}: {error}")
        return 2
    except ValueError as error:
        _say(f"traza {args.name}: {error}")
        return 2


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
