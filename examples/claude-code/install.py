#!/usr/bin/env python3
"""Installs the Traza hook pack into a Claude Code settings file.

    python3 examples/claude-code/install.py                 # this project
    python3 examples/claude-code/install.py --project DIR   # another project
    python3 examples/claude-code/install.py --user          # every project
    python3 examples/claude-code/install.py --uninstall

What it does, and nothing else:

1. copies traza_hook.py to <claude dir>/hooks/traza/ — `.claude/` in the
   project, or `~/.claude/` with --user — so the settings never point into
   a Traza checkout that may move;
2. writes <claude dir>/hooks/traza/config.json with the options given here
   (the URL, the tenant, a token FILE's path — never a token itself);
3. merges a `hooks` block into <claude dir>/settings.json (or
   settings.local.json with --local): PreToolUse and SessionStart, plus
   PostToolUse and PostToolUseFailure with --record-tools. Every other
   setting and every other hook is kept as it was; an earlier Traza entry is
   replaced, so running it twice changes nothing. The previous file is kept
   beside it as <file>.traza-backup.

A settings file that is not valid JSON is refused, never overwritten.
--dry-run prints the merged settings instead of writing anything.

The hook contract this targets is https://code.claude.com/docs/en/hooks.
Standard library only; Python 3.8 or later.
"""

import argparse
import json
import os
import shutil
import sys

MARKER = "traza_hook.py"
HOOK_TIMEOUT_SECONDS = 10


def shell_quote(path):
    """`path` inside double quotes for the shell-form command Claude Code
    runs, with the characters double quotes do not protect escaped."""
    escaped = "".join("\\" + c if c in '"\\$`' else c for c in path)
    return '"%s"' % escaped


def command_for(claude_dir, user, python):
    if user:
        script = os.path.join(claude_dir, "hooks", "traza", MARKER)
        return "%s %s" % (python, shell_quote(script))
    # Portable across clones: Claude Code sets CLAUDE_PROJECT_DIR to the
    # project root for every hook it runs.
    return '%s "${CLAUDE_PROJECT_DIR}/.claude/hooks/traza/%s"' % (python, MARKER)


def is_ours(handler):
    return isinstance(handler, dict) and MARKER in str(handler.get("command", ""))


def without_ours(groups):
    """`groups` (one event's matcher groups) with every Traza handler taken
    out, and groups left empty by that removed."""
    kept = []
    for group in groups if isinstance(groups, list) else []:
        if not isinstance(group, dict):
            kept.append(group)
            continue
        handlers = group.get("hooks")
        if not isinstance(handlers, list):
            kept.append(group)
            continue
        remaining = [handler for handler in handlers if not is_ours(handler)]
        if remaining:
            group = dict(group)
            group["hooks"] = remaining
            kept.append(group)
        elif len(remaining) == len(handlers):
            kept.append(group)
    return kept


def merged(settings, events, command):
    """`settings` with Traza's handlers for `events` (none: uninstall)."""
    result = dict(settings)
    hooks = dict(result.get("hooks") or {})
    for event in list(hooks):
        groups = without_ours(hooks[event])
        if groups:
            hooks[event] = groups
        else:
            del hooks[event]
    for event in events:
        group = {"hooks": [{"type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECONDS}]}
        if event != "SessionStart":
            # SessionStart matches on how the session began; every way.
            group = {"matcher": "*", **group}
        hooks.setdefault(event, []).append(group)
    if hooks:
        result["hooks"] = hooks
    else:
        result.pop("hooks", None)
    return result


def write_atomically(path, text):
    staged = path + ".traza-tmp"
    with open(staged, "w", encoding="utf-8") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(staged, path)


def main(argv=None):
    here = os.path.dirname(os.path.abspath(__file__))
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    where = parser.add_mutually_exclusive_group()
    where.add_argument("--project", metavar="DIR", help="the project to install into (default: .)")
    where.add_argument("--user", action="store_true", help="install into ~/.claude, every project")
    parser.add_argument("--local", action="store_true",
                        help="write settings.local.json (not committed) instead of settings.json")
    parser.add_argument("--url", help="the Traza server (default http://127.0.0.1:8080)")
    parser.add_argument("--tenant", help="the runs' tenant, for an unbound token")
    parser.add_argument("--token-file", help="a file holding the bearer token (never the token)")
    parser.add_argument("--fail-closed", action="store_true",
                        help="deny tool calls when Traza cannot answer (default: let them through)")
    parser.add_argument("--no-recall", action="store_true",
                        help="no SessionStart summary of earlier runs")
    parser.add_argument("--recall-query", help="words earlier runs must contain (default: the "
                                                "project directory's name)")
    parser.add_argument("--record-tools", action="store_true",
                        help="also record each tool call as a span (only without another recorder)")
    parser.add_argument("--python", default="python3", help="the interpreter hooks run under")
    parser.add_argument("--uninstall", action="store_true", help="remove the Traza hooks")
    parser.add_argument("--dry-run", action="store_true", help="print the merged settings only")
    options = parser.parse_args(argv)

    if options.user:
        claude_dir = os.path.join(os.path.expanduser("~"), ".claude")
    else:
        claude_dir = os.path.join(os.path.abspath(options.project or "."), ".claude")
    settings_path = os.path.join(claude_dir, "settings.local.json" if options.local else "settings.json")
    hooks_dir = os.path.join(claude_dir, "hooks", "traza")

    settings = {}
    if os.path.exists(settings_path):
        try:
            with open(settings_path, encoding="utf-8") as handle:
                settings = json.load(handle)
        except ValueError as error:
            sys.stderr.write("%s is not valid JSON (%s); fix it first — nothing was changed\n"
                             % (settings_path, error))
            return 2
        if not isinstance(settings, dict):
            sys.stderr.write("%s is not a JSON object; nothing was changed\n" % settings_path)
            return 2

    events = []
    if not options.uninstall:
        events.append("PreToolUse")
        if not options.no_recall:
            events.append("SessionStart")
        if options.record_tools:
            events.extend(["PostToolUse", "PostToolUseFailure"])
    command = command_for(claude_dir, options.user, options.python)
    result = merged(settings, events, command)
    text = json.dumps(result, indent=2, ensure_ascii=False) + "\n"

    if options.dry_run:
        sys.stdout.write(text)
        return 0

    os.makedirs(claude_dir, exist_ok=True)
    if os.path.exists(settings_path):
        shutil.copy2(settings_path, settings_path + ".traza-backup")
    write_atomically(settings_path, text)

    if options.uninstall:
        shutil.rmtree(hooks_dir, ignore_errors=True)
        print("removed the Traza hooks from %s" % settings_path)
        return 0

    os.makedirs(hooks_dir, exist_ok=True)
    target = os.path.join(hooks_dir, MARKER)
    shutil.copy2(os.path.join(here, MARKER), target)
    os.chmod(target, 0o755)
    config = {}
    if options.url:
        config["url"] = options.url
    if options.tenant:
        config["tenant"] = options.tenant
    if options.token_file:
        config["token_file"] = os.path.abspath(os.path.expanduser(options.token_file))
    if options.fail_closed:
        config["fail_closed"] = True
    if options.no_recall:
        config["recall"] = False
    if options.recall_query:
        config["recall_query"] = options.recall_query
    if options.record_tools:
        config["record_tools"] = True
    write_atomically(os.path.join(hooks_dir, "config.json"), json.dumps(config, indent=2) + "\n")
    print("installed the Traza hooks (%s) into %s" % (", ".join(events), settings_path))
    print("hook script and config: %s" % hooks_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
