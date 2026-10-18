// Fork & Rewind from the Run screen: what the editor starts from, the
// replay it asks for, and the lines it hands back.
//
// A fork is a replay told where the recording stops being the truth
// (docs/guide/fork.md): an edited answer at a step, and optionally a live
// point after which every model call answers `409 replay_live` so the
// agent's harness goes to the real provider. The server never calls a
// model; the editor's job is to build the request precisely and say what
// happens next, in the words the server's own refusals use.

import { pyLiteral, replayEnv } from './runs.js';

/** What a step's answer editor opens with: the recorded answer, as text
    when it is plain text, else as the JSON it was recorded as.

    An llm step's response is a message list on the tape. One assistant
    message holding only text edits as that text — the common case, and the
    one where JSON punctuation is noise. Anything else (tool calls, several
    messages, content blocks) opens as JSON, so editing it keeps its shape.
    A tool step's result is any JSON; a string result edits as text. */
export function recordedAnswer(step) {
  const value = step ? step.response : undefined;
  if (value === undefined || value === null) return { format: step && step.kind === 'llm' ? 'text' : 'json', text: '' };
  if (value && typeof value === 'object' && !Array.isArray(value) && typeof value.$payload === 'string') {
    // Offloaded and not inlined: the editor cannot start from it.
    return { format: step.kind === 'llm' ? 'text' : 'json', text: '', offloaded: true };
  }
  if (typeof value === 'string') return { format: 'text', text: value };
  if (step.kind === 'llm' && Array.isArray(value) && value.length === 1) {
    const message = value[0];
    if (message && typeof message === 'object' && typeof message.content === 'string'
      && !(message.tool_calls && message.tool_calls.length)) {
      return { format: 'text', text: message.content };
    }
  }
  return { format: 'json', text: JSON.stringify(value, null, 2) };
}

/** Whether a step can be forked at all: model calls and tool calls. An agent
    step is neither an answer nor a result the replay serves. */
export function forkable(step) {
  return Boolean(step && (step.kind === 'llm' || step.kind === 'tool'));
}

/** The editor's value, parsed: `{value}` or `{error}`. */
export function editValue({ kind, format, text }) {
  if (format === 'json') {
    if (!text.trim()) return { error: 'Write the JSON to serve, or switch to text.' };
    try {
      return { value: JSON.parse(text) };
    } catch (e) {
      return { error: `That is not JSON (${e.message}). Switch to text to send it as written.` };
    }
  }
  if (kind === 'llm' && !text.trim()) {
    return { error: 'An edited answer needs text (or, as JSON, a tool call): the server refuses one with neither.' };
  }
  return { value: text };
}

/** The `POST /v1/replays` body for a fork at `step`, or `{error}`.

    `change` is `edit` (serve a different answer at the step) or `rewind`
    (replay everything before the step exactly, and go live from it). With
    an edit, `live` sets the live point at the step itself: the edit is
    served, and every call after it goes to the real provider. Without it,
    the recording plays on, loosely, after the edit. `tenant` undefined is
    the operator view, `''` the default tenant — as on every run route. */
export function buildForkRequest({ run, tenant, step, change, kind, format, text, live, mode }) {
  const body = { run, mode: mode === 'loose' ? 'loose' : 'strict' };
  if (tenant !== undefined) body.tenant = tenant;
  if (change === 'rewind') {
    if (!(step >= 1)) return { error: 'Rewinding to the first step replays nothing: the run would be live from its first call.' };
    body.live_after = step - 1;
    return { body };
  }
  const parsed = editValue({ kind, format, text });
  if (parsed.error) return { error: parsed.error };
  body.edits = [kind === 'llm' ? { step, response: parsed.value } : { step, tool_result: parsed.value }];
  if (live) body.live_after = step;
  return { body };
}

/** The environment a harness of your own runs a fork under: the replay's
    base URLs and keys (as for any replay — the token is never printed), and
    the four values its spans must carry so the fork is found again: its own
    session id and the `traza.fork.*` attributes. */
export function forkEnv(replay, base, auth) {
  const fork = replay.fork || {};
  const lines = [replayEnv(replay, base, auth)];
  lines.push(
    '# Give the forked run its own session id, and stamp these on every span:',
    `#   traza.fork.parent=${fork.parent}  traza.fork.step=${fork.step}  traza.fork.id=${fork.id}`,
    `export TRAZA_FORK_ID=${fork.id}`,
    `export TRAZA_FORK_PARENT=${shellWord(fork.parent || '')}`,
    `export TRAZA_FORK_STEP=${fork.step}`,
    `export TRAZA_FORK_RUN=${fork.id}`,
  );
  if (fork.live_after != null) {
    lines.push(`# Past step ${fork.live_after} every model call answers 409 replay_live: send it, and every later one, to the real provider.`);
  }
  return lines.join('\n');
}

/** The same fork from Python, where `traza.fork` does the switching and the
    span tagging. It makes a fork of its own with the same edit — the replay
    above is for a harness that does those itself. */
export function forkPython({ run, step, change, kind, value, live, mode }) {
  const args = [JSON.stringify(run), `at_step=${step}`];
  if (change !== 'rewind') {
    args.push(`${kind === 'llm' ? 'response' : 'tool_result'}=${pyLiteral(value)}`);
    if (!live) args.push('live=False');
  }
  if (mode === 'loose') args.push('mode="loose"');
  return [
    'import traza',
    '',
    `with traza.fork(${args.join(', ')}) as fork:`,
    '    run_agent()  # build the OpenAI / Anthropic client inside the block',
    'print(fork.id, fork.final_status.fork.live_calls)',
    'print(fork.diff().first_divergence.explanation)',
  ].join('\n');
}

/** The same fork as one shell command, around any agent command. */
export function forkCli({ run, step, change, kind, value, live, format }) {
  const parts = ['traza fork', shellWord(run), '--at', String(step)];
  if (change !== 'rewind') {
    if (kind === 'llm') {
      if (format === 'text' && typeof value === 'string') parts.push('--response', shellWord(value));
      else parts.push('--response-json', shellWord(JSON.stringify(value)));
    } else {
      parts.push('--tool-result', shellWord(JSON.stringify(value)));
    }
    if (!live) parts.push('--no-live');
  }
  parts.push('--', 'python agent.py');
  return parts.join(' ');
}

/** A word the shell reads back exactly: bare when it is safe, else single
    quoted with each quote closed, escaped and reopened. */
export function shellWord(text) {
  const value = String(text);
  if (/^[A-Za-z0-9_./:@%+=,-]+$/.test(value)) return value;
  return "'" + value.replace(/'/g, "'\\''") + "'";
}

/** One fork of the listing, read for display: how it ended against its
    parent, and where it parted. */
export function forkSummary(fork, parent) {
  const delta = fork.delta || null;
  return {
    run: fork.run,
    tenant: fork.tenant,
    id: fork.fork_id,
    step: fork.fork_step,
    diffed: Boolean(fork.diffed),
    skipped: fork.skipped || null,
    outcome: fork.outcome || null,
    parentOutcome: parent ? parent.outcome : null,
    steps: fork.steps,
    cost: fork.cost_usd,
    delta,
    divergence: fork.first_divergence || null,
    beforeCut: fork.diverged_before_cut === true,
  };
}
