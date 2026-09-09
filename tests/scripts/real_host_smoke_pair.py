#!/usr/bin/env python3
"""Require real Cursor + Codex evidence on the same binary; either failure closes the gate."""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import sqlite3
import codex_checkpoint_model_probe as codex_probe

def require(condition, message='real-host pair criterion failed'):
    if not condition:
        raise ValueError(message)

def read(path):
    return json.loads(path.read_text())

def finish_codex_lifecycle(directory):
    summary = read(directory / 'summary.json')
    require(summary['newly_accepted_submissions'] == 1, 'real-host pair criterion failed')
    binary = directory / 'home/.shared-context/bin/current/sctx'
    env = dict(os.environ, HOME=str(directory / 'home'), SCTX_SKIP_LAUNCHCTL='1')
    session = summary['results'][0]['session_id']
    base = {'session_id': session, 'transcript_path': None, 'cwd': str(directory / 'checkout')}
    stop = dict(base, hook_event_name='Stop', model=summary['model'], permission_mode='default', turn_id='paired-smoke-stop', stop_hook_active=False, last_assistant_message='Synthetic acceptance complete')
    end = dict(base, hook_event_name='SessionEnd', reason='other')

    def invoke(payload):
        result = subprocess.run([str(binary), 'hook', '--agent', 'codex', '--agent-version', summary['codex_cli_version']], env=env, input=json.dumps(payload), text=True, capture_output=True, check=True, timeout=30)
        return json.loads(result.stdout)
    result = {'delivery': 'test-driver, not native Codex hooks', 'stop': invoke(stop), 'repeated_stops': [invoke(stop) for _ in range(8)], 'session_end': invoke(end)}
    with sqlite3.connect(directory / 'home/.shared-context/state/runtime.sqlite') as db:
        result['builds'] = db.execute('SELECT status FROM candidate_build').fetchall()
        result['candidate_relations'] = db.execute('SELECT top_relation FROM candidate_review').fetchall()
    result['remaining_lease_files'] = [p.name for p in (directory / 'home/.shared-context/state/authorized-session-scopes').glob('*')]
    (directory / 'lifecycle-smoke.json').write_text(json.dumps(result, indent=2) + '\n')

def verify(codex, cursor):
    c = read(codex / 'summary.json')
    u = read(cursor / 'summary.json')
    host = [json.loads(line) for line in (cursor / 'host.jsonl').read_text().splitlines() if line.strip()]
    require(any((item.get('type') == 'result' and item.get('subtype') == 'success' and (not item.get('is_error')) for item in host)), 'Cursor host did not finish successfully')
    hooks = [json.loads(line) for line in (cursor / 'native-hooks.jsonl').read_text().splitlines() if line.strip()]
    require(any((item['input']['hook_event_name'] == 'sessionEnd' and item['output'] == {} for item in hooks)), 'Missing native Cursor SessionEnd cleanup')
    requests = [json.loads(line) for line in (cursor / 'mcp-input.jsonl').read_text().splitlines() if line.strip()]
    require([item['params']['name'] for item in requests if item.get('method') == 'tools/call'] == ['task_checkpoint', 'candidate_list'], 'Cursor wire did not contain the expected real calls')
    life = read(codex / 'lifecycle-smoke.json')
    require(c['trials'] == 1 and c['newly_accepted_submissions'] == 1 and c['checkpoint_git_unchanged'], 'real-host pair criterion failed')
    proof = codex_probe.evaluate_trial(0, c['results'][0]['session_id'], codex / 'raw/trial-000.jsonl', 0)
    require(proof.accepted and proof.legal, proof.failures)
    require(u['passed'] is True, 'Cursor did not pass; Codex alone cannot satisfy the gate')
    require(u['tool_calls'] == ['task_checkpoint', 'candidate_list'], 'real-host pair criterion failed')
    require(u['builds'] == [['complete']] and u['candidate_relations'] == [['novel']], 'real-host pair criterion failed')
    require({'sessionStart', 'sessionEnd'} <= set(u['native_hook_events']) and (not u['remaining_lease_files']), 'real-host pair criterion failed')
    require(life['builds'] == [['complete']] and life['candidate_relations'] == [['novel']], 'real-host pair criterion failed')
    require(life['session_end'] == {} and life['repeated_stops'] == [{}] * 8 and (not life['remaining_lease_files']), 'real-host pair criterion failed')
    digests = [hashlib.sha256((path / 'home/.shared-context/bin/current/sctx').read_bytes()).hexdigest() for path in [codex, cursor]]
    require(digests[0] == digests[1] == u['binary_sha256'], 'Hosts used different builds')
    wire = [json.loads(line) for line in (cursor / 'mcp-output.jsonl').read_text().splitlines() if line.strip()]
    tools = next((item['result']['tools'] for item in wire if isinstance(item.get('result'), dict) and 'tools' in item['result']))
    require(len(tools) == c['public_tool_count'] == 17, 'real-host pair criterion failed')
    schema = next((tool['inputSchema'] for tool in tools if tool['name'] == 'task_checkpoint'))
    digest = hashlib.sha256(json.dumps(schema, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    require(digest == c['schema_sha256'], 'Checkpoint schemas differ')
    return {'passed': True, 'binary_sha256': digests[0], 'checkpoint_schema_sha256': digest, 'codex': {'version': c['codex_cli_version'], 'model': c['model'], 'real_model_checkpoint': True, 'lifecycle_delivery': life['delivery'], 'builder': 'complete', 'leases_cleared': True}, 'cursor': {'version': u['agent_version'], 'models': u['host_models'], 'real_model_checkpoint': True, 'real_model_candidate_list': True, 'native_hooks': u['native_hook_events'], 'native_stop_observed': 'stop' in u['native_hook_events'], 'builder': 'complete', 'leases_cleared': True}, 'normal_use_sample': False, 'desktop_ui_coverage': False}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sctx-bin', type=pathlib.Path)
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--codex-dir', type=pathlib.Path)
    parser.add_argument('--cursor-dir', type=pathlib.Path)
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    # A failed rerun must never leave a previous green receipt in the output directory.
    (output / 'paired-summary.json').write_text(json.dumps({'passed': False, 'status': 'incomplete'}) + '\n')
    codex = (args.codex_dir or output / 'codex').resolve()
    cursor = (args.cursor_dir or output / 'cursor').resolve()
    scripts = pathlib.Path(__file__).resolve().parent
    if not args.verify_only:
        if not args.sctx_bin:
            parser.error('--sctx-bin is required for live runs')
        for script, directory, extra in [('cursor_checkpoint_model_smoke.py', cursor, []), ('codex_checkpoint_model_probe.py', codex, ['--trials', '1', '--parallelism', '1', '--skip-build'])]:
            subprocess.run([sys.executable, str(scripts / script), '--sctx-bin', str(args.sctx_bin.resolve()), '--output-dir', str(directory), *extra], check=True)
        finish_codex_lifecycle(codex)
    result = verify(codex, cursor)
    (output / 'paired-summary.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))
if __name__ == '__main__':
    main()
