#!/usr/bin/env python3
"""Real Cursor Agent/MCP smoke in a fresh private HOME; no production activation.

The existing macOS keychain is linked for Cursor's own authentication, never read by
this script. Raw host/MCP/hook evidence stays private in the chosen output directory.
SessionStart's harness bootstrap creates only the test Task; the real model submits
the Checkpoint. Native lifecycle delivery is reported separately from driver checks.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
import pathlib
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading


def run(argv, env=None, payload=None, cwd=None):
    return subprocess.run(argv, env=env, cwd=cwd, input=payload, text=True,
                          capture_output=True, check=True, timeout=120)


def hook(output):
    meta = json.loads((output / 'meta.json').read_text())
    payload = sys.stdin.read()
    value = json.loads(payload)
    result = run([meta['binary'], 'hook', '--agent', 'cursor', '--agent-version', meta['agent_version']], payload=payload)
    record = {'input': value, 'output': json.loads(result.stdout)}
    if value['hook_event_name'] == 'sessionStart' and 'shared-context-active' in result.stdout:
        session = value.get('session_id', value['conversation_id'])
        task = {'agent_kind': 'cursor', 'external_session_id': session, 'task_boundary': 'new',
                'expected_revision_id': None, 'intent': {'goal': 'Validate the real Cursor checkpoint contract'}}
        bootstrap = run([meta['binary'], '--json', 'task', 'intent', 'update', '--input', '/dev/stdin'], payload=json.dumps(task))
        record['driver_task_bootstrap'] = json.loads(bootstrap.stdout)
    with (output / 'native-hooks.jsonl').open('a') as stream:
        stream.write(json.dumps(record) + '\n')
    sys.stdout.write(result.stdout)


def proxy(output):
    meta = json.loads((output / 'meta.json').read_text())
    child = subprocess.Popen([meta['binary'], 'mcp', 'serve', '--client', 'cursor'],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr)
    def copy(source, destination, log):
        with log.open('ab') as stream:
            while chunk := os.read(source, 65536):
                stream.write(chunk)
                stream.flush()
                remaining = memoryview(chunk)
                while remaining:
                    remaining = remaining[os.write(destination, remaining):]
    def input_stream():
        try:
            copy(sys.stdin.fileno(), child.stdin.fileno(), output / 'mcp-input.jsonl')
        finally:
            child.stdin.close()
    thread = threading.Thread(target=input_stream, daemon=True)
    thread.start()
    copy(child.stdout.fileno(), sys.stdout.fileno(), output / 'mcp-output.jsonl')
    return child.wait()


def json_lines(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sctx-bin', type=pathlib.Path, required=True)
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    parser.add_argument('--agent-bin', default='agent')
    parser.add_argument('--model')
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    home = output / 'home'
    checkout = output / 'checkout'
    (home / '.cursor').mkdir(parents=True)
    (home / 'Library').mkdir()
    checkout.mkdir()
    # Cursor's default macOS keychain lookup uses HOME/Library/Keychains. No tokens
    # or keychain contents are copied, printed, exported, or passed as arguments.
    keychains = pathlib.Path.home() / 'Library/Keychains'
    if not keychains.is_dir():
        raise RuntimeError('existing macOS Cursor keychain is unavailable')
    (home / 'Library/Keychains').symlink_to(keychains, target_is_directory=True)
    config_path = pathlib.Path.home() / '.cursor/cli-config.json'
    original = json.loads(config_path.read_text())
    config = {key: original[key] for key in ['version', 'authInfo'] if key in original}
    (home / '.cursor/cli-config.json').write_text(json.dumps(config))
    (home / '.cursor/cli-config.json').chmod(0o600)
    env = dict(os.environ, HOME=str(home), SCTX_SKIP_LAUNCHCTL='1',
               SCTX_LOGS_ROOT=str(output / 'logs'), CURSOR_SCTX_SMOKE_ROOT=str(output),
               CURSOR_CONFIG_DIR=str(home / '.cursor'), CURSOR_DATA_DIR=str(home / '.cursor'),
               CURSOR_AGENT_STORE_DIR=str(output / 'agent-store'))
    agent = shutil.which(args.agent_bin)
    if not agent:
        raise RuntimeError('Cursor Agent executable not found')
    version = run([agent, '--version'], env=env).stdout.strip()
    status = run([agent, 'status'], env=env)
    if 'Logged in' not in status.stdout + status.stderr:
        raise RuntimeError('Cursor is not authenticated in the isolated HOME')
    run(['git', 'init', '-q', '-b', 'main', str(checkout)])
    source = args.sctx_bin.resolve()
    run([str(source), '--json', 'setup', '--agents', 'cursor', '--runtime-source', str(source),
         '--runtime-version', 'cursor-real-smoke'], env=env)
    binary = home / '.shared-context/bin/current/sctx'
    run([str(binary), 'repository', 'add', '--repository-id', 'CursorSmoke', '--path', str(checkout)], env=env)
    script = pathlib.Path(__file__).resolve()
    meta = {'binary': str(binary), 'agent_version': version, 'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest()}
    (output / 'meta.json').write_text(json.dumps(meta, indent=2))
    hooks_path = home / '.cursor/hooks.json'
    hooks = json.loads(hooks_path.read_text())
    for entries in hooks['hooks'].values():
        for entry in entries:
            entry['command'] = shlex.join([sys.executable, str(script), '--hook'])
    hooks_path.write_text(json.dumps(hooks, indent=2))
    mcp = {'mcpServers': {'shared-context': {'command': sys.executable, 'args': [str(script), '--mcp-proxy'],
           'env': {'HOME': str(home), 'CURSOR_SCTX_SMOKE_ROOT': str(output), 'SCTX_LOGS_ROOT': str(output / 'logs')}}}}
    (home / '.cursor/mcp.json').write_text(json.dumps(mcp, indent=2))
    prompt = '''This is an authorized synthetic acceptance smoke in an isolated installation.
The native SessionStart hook has supplied a shared-context-active marker and the harness
has bootstrapped its test Task. Use that exact marker session ID. Make exactly one ordinary
shared-context task_checkpoint MCP call, with agent_kind cursor, unknowns [], and one Claim:
context_kind discovery; statement "The real Cursor host can submit the flat checkpoint contract.";
rationale "An ordinary model-driven MCP request verifies this contract in an isolated fixture.";
conditions []; evidence [{evidence_type: "experiment_record", summary: "Real Cursor MCP smoke",
limitations: ["Synthetic acceptance fixture; not a normal-use quality sample"]}].
After checkpoint acceptance, call shared-context candidate_list exactly once with the same
agent_kind and external_session_id, status pending and detail_level full, to recover the queued
Builder. Omit cursor. Then finish without confirming or discarding. Do not use shell, edit,
read unrelated files or call any other business tool. Reading the installed shared-context
SKILL.md/workflow.md and host MCP tool-description discovery are allowed.'''
    argv = [agent, '--print', '--output-format', 'stream-json', '--auto-review', '--approve-mcps',
            '--trust', '--workspace', str(checkout)]
    if args.model:
        argv += ['--model', args.model]
    argv.append(prompt)
    with (output / 'host.jsonl').open('w') as stdout, (output / 'host.stderr').open('w') as stderr:
        child = subprocess.Popen(argv, cwd=checkout, env=env, stdin=subprocess.DEVNULL,
                                 stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            code = child.wait(timeout=240)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=15)
            code = 124
    requests = json_lines(output / 'mcp-input.jsonl')
    responses = json_lines(output / 'mcp-output.jsonl')
    calls = [item for item in requests if item.get('method') == 'tools/call']
    checkpoints = [item for item in calls if item['params']['name'] == 'task_checkpoint']
    results = {item.get('id'): item.get('result') for item in responses if 'id' in item}
    hook_records = json_lines(output / 'native-hooks.jsonl')
    summary = {'host': 'Cursor Agent CLI', 'agent_version': version, 'exit_code': code,
               'binary_sha256': meta['source_sha256'], 'model_requested': args.model or 'host default',
               'tool_calls': [item['params']['name'] for item in calls],
               'native_hook_events': [item['input']['hook_event_name'] for item in hook_records],
               'checkpoint_results': [results.get(item['id']) for item in checkpoints],
               'normal_use_sample': False, 'driver_bootstraps_task': True}
    database = home / '.shared-context/state/runtime.sqlite'
    with sqlite3.connect(database) as db:
        summary['checkpoints'] = db.execute('SELECT COUNT(*) FROM agent_checkpoint').fetchone()[0]
        summary['builds'] = db.execute('SELECT status FROM candidate_build').fetchall()
        summary['candidate_relations'] = db.execute('SELECT top_relation FROM candidate_review').fetchall()
    summary['remaining_lease_files'] = [p.name for p in (home / '.shared-context/state/authorized-session-scopes').glob('*')]
    host_events = json_lines(output / 'host.jsonl')
    inits = [item for item in host_events if item.get('type') == 'system' and item.get('subtype') == 'init']
    summary['host_models'] = sorted({item.get('model', 'unreported') for item in inits})
    summary['host_session_ids'] = sorted({item['session_id'] for item in inits})
    host_tool_kinds = [key for item in host_events if item.get('type') == 'tool_call' and item.get('subtype') == 'started'
                       for key in item.get('tool_call', {}) if key.endswith('ToolCall')]
    summary['host_tool_kinds'] = host_tool_kinds
    reads = [item['tool_call']['readToolCall']['args']['path'] for item in host_events
             if item.get('type') == 'tool_call' and item.get('subtype') == 'started'
             and 'readToolCall' in item.get('tool_call', {})]
    allowed_reads = {str(home / '.agents/skills/shared-context/SKILL.md'),
                     str(home / '.agents/skills/shared-context/references/workflow.md')}
    summary['skill_reads'] = reads
    summaries = [result.get('structuredContent', {}) for result in summary['checkpoint_results'] if result]
    summary['passed'] = (code == 0 and summary['tool_calls'] == ['task_checkpoint', 'candidate_list']
        and summary['checkpoints'] == 1 and summary['builds'] == [('complete',)]
        and len(summary['candidate_relations']) == 1 and not summary['remaining_lease_files']
        and {'sessionStart', 'sessionEnd'} <= set(summary['native_hook_events'])
        and len(summary['host_session_ids']) == 1
        and all(item['params']['arguments'].get('agent_kind') == 'cursor'
                and item['params']['arguments'].get('external_session_id') == summary['host_session_ids'][0] for item in calls)
        and len(summaries) == 1 and summaries[0].get('status') == 'accepted' and summaries[0].get('replayed') is False
        and all(results.get(item['id']) and not results[item['id']].get('isError') for item in calls)
        and all(kind in ['getMcpToolsToolCall', 'mcpToolCall', 'readToolCall'] for kind in host_tool_kinds)
        and all(path in allowed_reads for path in reads))
    (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))
    return 0 if summary['passed'] else 1


if __name__ == '__main__':
    if sys.argv[1:] == ['--hook']:
        hook(pathlib.Path(os.environ['CURSOR_SCTX_SMOKE_ROOT']))
    elif sys.argv[1:] == ['--mcp-proxy']:
        sys.exit(proxy(pathlib.Path(os.environ['CURSOR_SCTX_SMOKE_ROOT'])))
    else:
        sys.exit(main())
