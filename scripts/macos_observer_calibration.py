"""Bounded evidence validation for the supervisor's no-input calibration mode.

Raw counters are process/interval observations, never proof of user inactivity.
No calibration result authorizes or retries native input.
"""
import math

COUNTERS = {'any_input', 'key_down', 'key_up', 'mouse_move', 'scroll'}
ACCESS = {'listen_event_access', 'post_event_access', 'accessibility_trusted',
          'secure_event_input', 'console_user_matches'}
ZERO_EFFECTS = {'native_input_calls', 'windows_created', 'browsers_launched', 'event_taps_installed'}


def require(value, reason):
    if not value:
        raise ValueError(reason)


def integer(value, lower=0, upper=2**32-1):
    return type(value) is int and lower <= value <= upper


def finite(value):
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(value) and value >= 0
    except OverflowError:
        # Bounded JSON lines can still hold integers too large for a float.
        return False


def validate_record(record):
    require(isinstance(record, dict), 'record must be an object')
    kind = record.get('kind')
    if kind == 'ready':
        require(set(record) == {'kind', 'schema', 'profile', 'pid', 'seconds', 'period_ms', 'utc', 'os', 'access'} | ZERO_EFFECTS,
                'ready schema')
        require(integer(record['schema'], 1, 1) and record['profile'] == 'input_observer_calibration'
                and integer(record['pid'], 1) and integer(record['seconds'], 1, 30)
                and integer(record['period_ms'], 100, 100), 'ready identity or limits')
        require(isinstance(record['os'], str) and 0 < len(record['os']) <= 256, 'OS metadata')
    elif kind == 'started':
        require(set(record) == {'kind', 'utc', 'started_uptime'} and finite(record['started_uptime']), 'start schema')
    elif kind == 'sample':
        require(set(record) == {'kind', 'sequence', 'started_uptime', 'finished_uptime', 'hid_system', 'combined_session', 'access'}, 'sample schema')
        require(integer(record['sequence'], 0, 300) and finite(record['started_uptime'])
                and finite(record['finished_uptime']) and record['finished_uptime'] >= record['started_uptime'], 'sample ordering')
        for source in ('hid_system', 'combined_session'):
            value = record[source]
            require(isinstance(value, dict) and set(value) == COUNTERS
                    and all(integer(v) for v in value.values()), 'raw counter schema')
    elif kind == 'finished':
        require(set(record) == {'kind', 'completed', 'samples', 'reason', 'utc', 'finished_uptime'} | ZERO_EFFECTS, 'finish schema')
        require(type(record['completed']) is bool and integer(record['samples'], 0, 301)
                and finite(record['finished_uptime']) and isinstance(record['reason'], str) and record['reason'] in {
                    'finished', 'start_deadline', 'stdin_closed_before_start', 'cancelled_before_start',
                    'stdin_error', 'sampling_deadline', 'cancelled'}, 'finish values')
        require(record['completed'] == (record['reason'] == 'finished'), 'finish contradiction')
    else:
        raise ValueError('unknown record kind')
    if 'utc' in record:
        from datetime import datetime
        require(isinstance(record['utc'], str) and len(record['utc']) <= 40 and record['utc'].endswith('Z'), 'UTC timestamp')
        datetime.fromisoformat(record['utc'].replace('Z', '+00:00'))
    if 'access' in record:
        access = record['access']
        require(isinstance(access, dict) and set(access) == ACCESS, 'access schema')
        require(all(type(access[k]) is bool for k in ACCESS - {'secure_event_input', 'console_user_matches'}), 'preflight types')
        require(all(access[k] is None or type(access[k]) is bool for k in ('secure_event_input', 'console_user_matches')), 'nullable context')
    for key in ZERO_EFFECTS & set(record):
        require(integer(record[key], 0, 0), 'calibration reported an effect')
    return record


def summarize(records):
    require(1 <= len(records) <= 304, 'record count')
    for row in records:
        validate_record(row)
    require(records[0]['kind'] == 'ready', 'missing first ready record')
    ready = records[0]
    start = records[1] if len(records) > 1 and records[1]['kind'] == 'started' else None
    ended = records[-1] if records[-1]['kind'] == 'finished' else None
    samples = records[2:-1 if ended else None] if start else []
    if not start:
        require(len(records) == (2 if ended else 1), 'unexpected pre-start record')
    require(all(r['kind'] == 'sample' for r in samples), 'unexpected interior record')
    last = start['started_uptime'] if start else 0
    for i, row in enumerate(samples):
        require(row['sequence'] == i and row['started_uptime'] >= last, 'sample sequence/time gap')
        last = row['finished_uptime']
    if ended:
        require(ended['samples'] == len(samples) and ended['finished_uptime'] >= last, 'finish sample mismatch')
        if ended['completed']:
            require(start and len(samples) == ready['seconds'] * 10 + 1, 'incomplete completed series')
            elapsed = samples[-1]['finished_uptime'] - start['started_uptime']
            require(ready['seconds'] - .05 <= elapsed <= ready['seconds'] + 2.5, 'sampling interval outside bounds')
    completed = bool(ended and ended['completed'])
    sources = {}
    for source in ('hid_system', 'combined_session'):
        regression = any(b[source][k] < a[source][k] for a, b in zip(samples, samples[1:]) for k in COUNTERS)
        deltas = ({k: samples[-1][source][k] - samples[0][source][k] for k in sorted(COUNTERS)}
                  if len(samples) >= 2 and not regression else None)
        sources[source] = {'counter_regression': regression, 'deltas': deltas,
                           'keyboard_progress_observed': deltas['key_down'] > 0 and deltas['key_up'] > 0 if deltas else None}
    contexts = [ready['access']] + [r['access'] for r in samples]
    access_consistent = all(r['listen_event_access'] is True and r['secure_event_input'] is False
                            and r['console_user_matches'] is True for r in contexts)
    a, b = (sources[k]['keyboard_progress_observed'] for k in ('hid_system', 'combined_session'))
    outcome = ('incomplete' if not completed else 'counter_regression' if any(s['counter_regression'] for s in sources.values())
               else 'keyboard_progress_both' if a and b else 'keyboard_progress_hid_only' if a
               else 'keyboard_progress_session_only' if b else 'no_keyboard_progress_observed')
    return {'completed': completed, 'samples': len(samples), 'outcome': outcome, 'sources': sources,
            'access_context_consistent': access_consistent, 'access_context_changed': any(r != contexts[0] for r in contexts),
            'user_inactivity_established': False, 'user_typing_confirmed': None,
            'human_activity_attribution': 'not_authenticated', 'input_delivery_verified': False,
            'continuous_isolation_verified': False}
