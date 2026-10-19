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
2. writes <claude dir>/hooks/traza/config.json: the configuration already
   there, changed only by the options given on this run (the URL, the
   tenant, a token FILE's path — never a token itself). Re-running with one
   option changes that option and keeps the rest; --reset starts from the
   defaults instead;
3. merges a `hooks` block into <claude dir>/settings.json (or
   settings.local.json with --local): PreToolUse, SessionStart unless recall
   is off, and PostToolUse and PostToolUseFailure when tool recording is on
   — as the merged configuration says. Every other setting and every other
   hook is kept as it was; an earlier Traza entry is replaced, so running it
   twice changes nothing. The settings as they were before Traza's first
   install are kept beside them as <file>.traza-backup, and a later run
   never overwrites that copy with settings Traza already changed.

A settings file that is not valid JSON is refused, never overwritten.
--dry-run prints the merged settings and configuration instead of writing
anything.

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


def has_ours(settings):
    """Whether `settings` already carries a Traza handler."""
    hooks = settings.get("hooks") if isinstance(settings, dict) else None
    if not isinstance(hooks, dict):
        return False
    for groups in hooks.values():
        for group in groups if isinstance(groups, list) else []:
            handlers = group.get("hooks") if isinstance(group, dict) else None
            if isinstance(handlers, list) and any(is_ours(handler) for handler in handlers):
                return True
    return False


def read_config(path):
    """The configuration an earlier run wrote, or nothing."""
    try:
        with open(path, encoding="utf-8") as handle:
            stored = json.load(handle)
    except (OSError, ValueError):
        return {}
    return stored if isinstance(stored, dict) else {}


def configured(stored, options):
    """`stored` changed by the options given on this run, and nothing else.
    An empty --url, --tenant, --token-file or --recall-query removes it."""
    config = {} if options.reset else dict(stored)

    def put(key, value):
        if value in (None, ""):
            config.pop(key, None)
        else:
            config[key] = value

    if options.url is not None:
        put("url", options.url)
    if options.tenant is not None:
        put("tenant", options.tenant)
    if options.token_file is not None:
        put("token_file", os.path.abspath(os.path.expanduser(options.token_file))
            if options.token_file else "")
    if options.recall_query is not None:
        put("recall_query", options.recall_query)
    # Switches: given, they set the value; absent, the stored one stands.
    # Defaults are left out of the file, so the hook's defaults apply.
    for key, on, off, default in (
        ("fail_closed", options.fail_closed, options.fail_open, False),
        ("recall", options.recall, options.no_recall, True),
        ("recall_steps", options.recall_steps, options.no_recall_steps, False),
        ("record_tools", options.record_tools, options.no_record_tools, False),
    ):
        if on or off:
            value = bool(on)
            if value == default:
                config.pop(key, None)
            else:
                config[key] = value
    return config


def switched_on(config, key, default):
    """A switch as the hook reads it: a boolean, or a string such as "1"."""
    value = config.get(key, default)
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("1", "true", "yes", "on")


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
    closed = parser.add_mutually_exclusive_group()
    closed.add_argument("--fail-closed", action="store_true",
                        help="deny tool calls when Traza cannot answer (default: let them through)")
    closed.add_argument("--fail-open", action="store_true",
                        help="let tool calls through when Traza cannot answer (the default)")
    recall = parser.add_mutually_exclusive_group()
    recall.add_argument("--recall", action="store_true",
                        help="a SessionStart summary of earlier runs (the default)")
    recall.add_argument("--no-recall", action="store_true",
                        help="no SessionStart summary of earlier runs")
    parser.add_argument("--recall-query", help="words earlier runs must contain (default: the "
                                                "project directory's name)")
    names = parser.add_mutually_exclusive_group()
    names.add_argument("--recall-steps", action="store_true",
                       help="name earlier runs' steps in the summary (recorded telemetry text)")
    names.add_argument("--no-recall-steps", action="store_true",
                       help="step counts by kind only (the default)")
    record = parser.add_mutually_exclusive_group()
    record.add_argument("--record-tools", action="store_true",
                        help="also record each tool call as a span (only without another recorder)")
    record.add_argument("--no-record-tools", action="store_true",
                        help="do not record tool calls (the default)")
    parser.add_argument("--reset", action="store_true",
                        help="start from the defaults, not the configuration an earlier run wrote")
    parser.add_argument("--python", default="python3", help="the interpreter hooks run under")
    parser.add_argument("--uninstall", action="store_true", help="remove the Traza hooks")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the merged settings and configuration only")
    options = parser.parse_args(argv)

    if options.user:
        claude_dir = os.path.join(os.path.expanduser("~"), ".claude")
    else:
        claude_dir = os.path.join(os.path.abspath(options.project or "."), ".claude")
    settings_path = os.path.join(claude_dir, "settings.local.json" if options.local else "settings.json")
    hooks_dir = os.path.join(claude_dir, "hooks", "traza")
    config_path = os.path.join(hooks_dir, "config.json")

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

    config = configured(read_config(config_path), options)
    events = []
    if not options.uninstall:
        events.append("PreToolUse")
        if switched_on(config, "recall", True):
            events.append("SessionStart")
        if switched_on(config, "record_tools", False):
            events.extend(["PostToolUse", "PostToolUseFailure"])
    command = command_for(claude_dir, options.user, options.python)
    result = merged(settings, events, command)
    text = json.dumps(result, indent=2, ensure_ascii=False) + "\n"
    config_text = json.dumps(config, indent=2, sort_keys=True) + "\n"

    if options.dry_run:
        sys.stdout.write(text)
        if not options.uninstall:
            sys.stdout.write("%s:\n%s" % (config_path, config_text))
        return 0

    os.makedirs(claude_dir, exist_ok=True)
    backup = settings_path + ".traza-backup"
    if os.path.exists(settings_path) and (not has_ours(settings) or not os.path.exists(backup)):
        # The settings as they were before Traza: a later run, reading
        # settings Traza already changed, must not replace that copy.
        shutil.copy2(settings_path, backup)
    write_atomically(settings_path, text)

    if options.uninstall:
        shutil.rmtree(hooks_dir, ignore_errors=True)
        print("removed the Traza hooks from %s" % settings_path)
        return 0

    os.makedirs(hooks_dir, exist_ok=True)
    target = os.path.join(hooks_dir, MARKER)
    shutil.copy2(os.path.join(here, MARKER), target)
    os.chmod(target, 0o755)
    write_atomically(config_path, config_text)
    print("installed the Traza hooks (%s) into %s" % (", ".join(events), settings_path))
    print("hook script and config: %s" % hooks_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
