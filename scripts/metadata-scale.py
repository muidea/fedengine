#!/usr/bin/env python3
"""Measure a real release process against an explicitly disposable PostgreSQL.

Requires psycopg2. Fixtures, process state and logs are removed on every exit.
A result is printed to stdout; the tool does not archive acceptance reports.
"""
import argparse
from contextlib import closing
import concurrent.futures
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

import psycopg2

ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def percentile(values, percent):
    ordered = sorted(values)
    return ordered[int((len(ordered) - 1) * percent / 100)]


def run(args, repetition):
    schemas = [f'fedscale_{uuid.uuid4().hex}' for _ in range(args.sources)]
    native = psycopg2.connect(os.environ[args.postgres_dsn_env])
    native.autocommit = True
    process = None
    stop = threading.Event()
    threads, samples, baseline, resources, rounds, slow_queries = [], [], [], [], [], []
    error = None
    result = None
    with tempfile.TemporaryDirectory(prefix='fedengine-scale-') as directory:
        work = Path(directory)
        try:
            fields = ','.join(f'c{i} integer' for i in range(args.columns))
            with native.cursor() as cursor:
                for schema in schemas:
                    cursor.execute(f"CREATE SCHEMA {schema}; DO $$ BEGIN FOR i IN 0..{args.objects - 1} LOOP EXECUTE format('CREATE TABLE {schema}.t_%s({fields})',i); END LOOP; END $$; INSERT INTO {schema}.t_0(c0) VALUES(7)")
            pg_port, api_port = free_port(), free_port()
            while pg_port == api_port:
                api_port = free_port()
            config = (ROOT / 'config/application.toml').read_text().split('# Optional explicit account database.')[0]
            replacements = {
                '127.0.0.1:15432': f'127.0.0.1:{pg_port}',
                '127.0.0.1:15433': f'127.0.0.1:{api_port}',
                './data/fedengine.duckdb': str(work / 'state.duckdb'),
                './data/tmp': str(work / 'tmp'),
                './secrets': str(work / 'secrets'),
                'memory_limit = "1GB"': 'memory_limit = "256MB"',
                'threads = 4': 'threads = 1',
                'metadata_operation_memory_mb = 256': f'metadata_operation_memory_mb = {args.operation_mib}',
            }
            for before, after in replacements.items():
                assert before in config, before
                config = config.replace(before, after)
            config = config.replace('[management]', f'extension_directory = {json.dumps(str(args.extensions))}\n[management]')
            config += '\n[[virtual_databases]]\nname="fedengine"\nowner="fedengine"\n'
            for index, schema in enumerate(schemas):
                for mapping in range(args.mappings):
                    config += f'[[virtual_databases.schemas]]\nname="business_{index}_{mapping}"\nsource="scale{index}"\nremote_schema="{schema}"\n'
            for index in range(args.sources):
                config += f'\n[[grants]]\nprincipal="fedengine"\nsource="scale{index}"\nschema="*"\nobject="*"\nactions=["SELECT"]\n'
            (work / 'config.toml').write_text(config)
            (work / 'secrets').mkdir()
            secret = work / 'secrets/pg.dsn'
            secret.write_text(os.environ[args.postgres_dsn_env])
            secret.chmod(0o600)
            env = dict(os.environ, FEDENGINE_PASSWORD='disposable-scale-only', RUST_LOG='warn,fedengine_duckdb=debug,fedengine_management=debug,fedengine_query=debug', NO_COLOR='1')
            if args.library_dir:
                env['LD_LIBRARY_PATH'] = str(args.library_dir) + os.pathsep + env.get('LD_LIBRARY_PATH', '')
            log_path = work / 'server.log'
            with log_path.open('w') as log:
                process = subprocess.Popen([str(args.binary), '--config', str(work / 'config.toml')], env=env, stdout=log, stderr=subprocess.STDOUT)
            base = f'http://127.0.0.1:{api_port}/api/v1/'

            def api(path, body=None):
                request = urllib.request.Request(base + path, data=json.dumps(body).encode() if body is not None else None, headers={'Content-Type': 'application/json', 'Idempotency-Key': uuid.uuid4().hex})
                with urllib.request.urlopen(request, timeout=30) as response:
                    return json.load(response)

            def finish(operation):
                end = time.monotonic() + 600
                while time.monotonic() < end:
                    state = api('operations/' + operation['id'])
                    if state['status'] in ['Succeeded', 'Failed', 'Interrupted']:
                        if state['status'] != 'Succeeded':
                            raise RuntimeError(f"operation {state['status']}: {state.get('error')}")
                        return
                    time.sleep(.1)
                raise TimeoutError('metadata operation exceeded 600s')

            def connect():
                connection = psycopg2.connect(host='127.0.0.1', port=pg_port, dbname='fedengine', user='fedengine', password='disposable-scale-only', connect_timeout=5)
                connection.autocommit = True
                return connection

            def reader(label, sql, expected):
                try:
                    with closing(connect()) as connection:
                        while not stop.is_set():
                            start = time.monotonic()
                            started_unix_ms = time.time() * 1000
                            failure = None
                            try:
                                with connection.cursor() as cursor:
                                    cursor.execute(sql)
                                    assert cursor.fetchone() == expected
                            except Exception as exc:
                                failure = type(exc).__name__ + ':' + str(exc)[:150]
                            elapsed = (time.monotonic() - start) * 1000
                            samples.append((elapsed, failure))
                            if elapsed > 200:
                                slow_queries.append(dict(reader=label, started_unix_ms=started_unix_ms, elapsed_ms=elapsed))
                            stop.wait(.02)
                except Exception as exc:
                    samples.append((0, type(exc).__name__))

            def monitor():
                while not stop.is_set():
                    try:
                        status = Path(f'/proc/{process.pid}/status').read_text()
                        values = dict(re.findall(r'^(VmRSS|VmHWM):\s+(\d+)', status, re.M))
                        if values:
                            resources.append((int(values['VmRSS']) / 1024, int(values['VmHWM']) / 1024))
                    except FileNotFoundError:
                        return
                    stop.wait(.02)

            for _ in range(600):
                if process.poll() is not None:
                    raise RuntimeError('release process exited before Ready')
                try:
                    api('sources')
                    break
                except (urllib.error.URLError, TimeoutError):
                    time.sleep(.1)
            else:
                raise TimeoutError('release did not become Ready')
            with closing(connect()) as connection:
                for _ in range(50):
                    start = time.monotonic()
                    with connection.cursor() as cursor:
                        cursor.execute('SELECT 1')
                        assert cursor.fetchone() == (1,)
                    baseline.append((time.monotonic() - start) * 1000)
                    time.sleep(.02)
            for function, parameters in [(reader, ('unrelated', 'SELECT 1', (1,))), (monitor, ())]:
                thread = threading.Thread(target=function, args=parameters)
                thread.start()
                threads.append(thread)
            for index, schema in enumerate(schemas):
                finish(api('sources', dict(id=f'scale{index}', kind='postgres', catalog_alias=f'scale{index}', secret_ref='file:pg.dsn', default_schema=schema, allowed_schemas=[schema], read_only=True)))
            identities = {}
            for round_number in range(args.rounds + 1):
                start = time.monotonic()
                def refresh(index):
                    state = api(f'sources/scale{index}')
                    finish(api(f'sources/scale{index}/' + ('apply' if round_number == 0 else 'refresh'), dict(target=dict(incarnation=state['incarnation'], expected_revision=state['desired_revision']))))
                with concurrent.futures.ThreadPoolExecutor(max_workers=args.sources) as executor:
                    list(executor.map(refresh, range(args.sources)))
                if round_number == 0:
                    for index in range(args.sources):
                        thread = threading.Thread(target=reader, args=(f'source_{index}', f'SELECT c0 FROM business_{index}_0.t_0', (7,)))
                        thread.start()
                        threads.append(thread)
                for index in range(args.sources):
                    page = api(f'sources/scale{index}/catalog?object=t_0&include=core,column_properties')
                    obj = page['objects'][0]
                    assert len(obj['columns']) == len(obj['metadata']['columns']) == args.columns
                    oid = obj['metadata']['oid']
                    assert identities.setdefault(index, oid) == oid
                    assert api(f'sources/scale{index}/catalog?limit=1')['total'] == args.objects
                # A committed operation is visible before its checkpoint worker
                # releases preparation resources. Measure the round-end plateau
                # after maintenance returns, while retaining the entire peak.
                deadline = time.monotonic() + 60
                expected_checkpoints = (round_number + 1) * args.sources
                while len(re.findall(r'metadata phase finished phase="?checkpoint"?\s', log_path.read_text())) < expected_checkpoints:
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise TimeoutError('post-publication maintenance did not drain')
                    time.sleep(.05)
                time.sleep(.2)
                state = dict(round=round_number, seconds=time.monotonic() - start, rss_mib=resources[-1][0], peak_rss_mib=max(r[1] for r in resources))
                rounds.append(state)
                print(json.dumps(dict(repetition=repetition, **state)), flush=True)
            with closing(connect()) as connection:
                with connection.cursor() as cursor:
                    cursor.execute("SELECT count(*) FROM pg_attribute a JOIN pg_class c ON a.attrelid=c.oid WHERE c.relkind='f' AND a.attnum>0 AND NOT a.attisdropped")
                    assert cursor.fetchone() == (args.objects * args.columns * args.sources * args.mappings,)
        except Exception as exc:
            error = type(exc).__name__ + ': ' + str(exc)
        finally:
            stop.set()
            for thread in threads:
                thread.join(5)
            readers_drained = all(not thread.is_alive() for thread in threads)
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(90)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            # Closing the process also releases blocked client reads on errors.
            # Keep the pre-shutdown drain verdict: killing a stuck worker is not
            # evidence of normal resource return.
            for thread in threads:
                thread.join(5)
            text = log_path.read_text() if 'log_path' in locals() and log_path.exists() else ''
            phase_metrics = {}
            phase_memory = {}
            for line in text.splitlines():
                observation = re.search(r'phase="?(\w+)"?.*?rss_kib=(\d+).*?peak_rss_kib=(\d+)', line)
                if observation:
                    phase, rss, hwm = observation.groups()
                    record = phase_memory.setdefault(phase, dict(observed_rss_mib=0, process_hwm_mib=0))
                    record['observed_rss_mib'] = max(record['observed_rss_mib'], int(rss) / 1024)
                    record['process_hwm_mib'] = max(record['process_hwm_mib'], int(hwm) / 1024)
                for name in ['catalog remote capture finished', 'catalog identity assignment finished', 'catalog local projection finished', 'management publication preparation finished', 'management instance publication barrier released', 'management live instance mutation finished', 'management local commit finished', 'management post-publication checkpoint finished']:
                    if name in line:
                        match = re.search(r'elapsed_ms=([\d.]+)', line)
                        if match:
                            phase_metrics.setdefault(name, []).append(float(match[1]))
            for name in list(phase_metrics):
                values = phase_metrics[name]
                phase_metrics[name] = dict(count=len(values), min_ms=min(values), max_ms=max(values))
            times = [s[0] for s in samples]
            last_rss = [r['rss_mib'] for r in rounds[-3:]]
            baseline_p99 = percentile(baseline, 99) if baseline else 0
            checks = dict(operations=error is None, zero_query_errors=bool(times) and all(s[1] is None for s in samples), max_pause=bool(times) and max(times) <= 1000, p99=bool(times) and percentile(times, 99) <= baseline_p99 + 100, rss=bool(resources) and max(r[1] for r in resources) <= args.max_rss_mib, rss_platform=len(last_rss) == 3 and max(last_rss) - min(last_rss) <= 64, graceful_shutdown=process is not None and process.returncode == 0, reader_drain=readers_drained)
            result = dict(repetition=repetition, objects=args.objects * args.sources, columns=args.objects * args.columns * args.sources, mappings=args.mappings, refresh_rounds=args.rounds, operation_mib=args.operation_mib, rss_limit_mib=args.max_rss_mib, checks=checks, error=error, query_count=len(times), query_errors=sum(s[1] is not None for s in samples), baseline_p99_ms=baseline_p99, query_p99_ms=percentile(times, 99) if times else None, query_max_ms=max(times, default=0), peak_rss_mib=max((r[1] for r in resources), default=0), last3_rss_spread_mib=max(last_rss, default=0) - min(last_rss, default=0), stages=phase_metrics, phase_memory=phase_memory)
            result['slow_queries'] = sorted(slow_queries, key=lambda s: s['elapsed_ms'], reverse=True)[:10]
            if error:
                # Sanitized tail is unnecessary: exception/status and phase data
                # identify failure without leaking source configuration in logs.
                print('scale failure:', error, flush=True)
            cleanup_errors = []
            try:
                with native.cursor() as cursor:
                    for schema in schemas:
                        try:
                            cursor.execute(f'DROP SCHEMA IF EXISTS {schema} CASCADE')
                        except psycopg2.Error as exc:
                            cleanup_errors.append(dict(schema=schema, sqlstate=exc.pgcode))
            finally:
                native.close()
            result['checks']['fixture_cleanup'] = not cleanup_errors
            if cleanup_errors:
                result['cleanup_errors'] = cleanup_errors
    print(json.dumps(dict(result=result)), flush=True)
    return all(result['checks'].values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/fedengine')
    parser.add_argument('--library-dir', type=Path)
    parser.add_argument('--extensions', type=Path, default=ROOT / 'data/extensions')
    parser.add_argument('--postgres-dsn-env', default='FEDENGINE_POSTGRES_DSN')
    parser.add_argument('--disposable-source', action='store_true', required=True, help='confirm the supplied DSN names a disposable test database')
    parser.add_argument('--objects', type=int, default=10000)
    parser.add_argument('--columns', type=int, default=10)
    parser.add_argument('--rounds', type=int, default=10)
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--sources', type=int, default=1)
    parser.add_argument('--mappings', type=int, default=1)
    parser.add_argument('--operation-mib', type=int, choices=[256, 512], default=512)
    parser.add_argument('--max-rss-mib', type=int, default=1024)
    args = parser.parse_args()
    if not (1 <= args.objects <= 10000 and 1 <= args.columns and args.objects * args.columns * args.sources <= 100000 and args.rounds >= 3 and args.repetitions >= 1 and 1 <= args.sources <= 4 and 1 <= args.mappings <= 4 and args.max_rss_mib > 0):
        parser.error('invalid scale dimensions or budget')
    args.binary = args.binary.resolve()
    args.extensions = args.extensions.resolve()
    if args.library_dir:
        args.library_dir = args.library_dir.resolve()
    if not args.binary.is_file() or not args.extensions.is_dir():
        parser.error('binary and locked extension directory must exist')
    for repetition in range(1, args.repetitions + 1):
        if not run(args, repetition):
            raise SystemExit(1)


if __name__ == '__main__':
    main()
