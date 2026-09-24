"""One disposable production key attempt with passive focus/HID witnesses.

This measures one fixed key, not an input retry policy or an isolation guarantee.
No human key contents, event taps, activation, or foreground mutations are used.
"""
import math
import time
import macos_arrow_acceptance as arrows
import macos_focus_witness as focus
import macos_receiver_study as study

COUNTERS = ('any_input', 'key_down', 'key_up', 'mouse_move', 'scroll')
require = study.require


def validate_options(enabled, keyboard, click, left, right, other=False):
    if enabled and (not keyboard or not click or left == right or other):
        raise ValueError('concurrent key study requires exactly one arrow and explicit receiver/click setup; excludes other studies and fixtures')


def validate_witness(value, sequence, phase):
    require(isinstance(value, dict) and 'hid_activity' in value, 'missing activity witness')
    core = {k: v for k, v in value.items() if k != 'hid_activity'}
    focus.validate(core, sequence, phase)
    activity = value['hid_activity']
    require(isinstance(activity, dict) and activity.get('source') == 'hid_system', 'activity source')
    if phase == 'before':
        require(set(activity) == {'source', 'sampled'} and activity['sampled'] is True,
                'activity baseline schema')
        return value
    require(set(activity) == {'source', 'counter_regression', 'deltas', 'changed', 'attribution'},
            'activity result schema')
    require(type(activity['counter_regression']) is bool and activity['attribution'] == 'not_authenticated',
            'activity attribution or regression type')
    deltas = activity['deltas']
    require(isinstance(deltas, dict) and set(deltas) == set(COUNTERS), 'activity counters')
    if activity['counter_regression']:
        require(activity['changed'] is None and all(v is None for v in deltas.values()),
                'regressed counters must be unknown')
    else:
        require(all(type(v) is int and 0 <= v <= 2**32-1 for v in deltas.values()), 'activity delta type/range')
        require(type(activity['changed']) is bool and activity['changed'] == any(deltas.values()),
                'activity change disagrees with counters')
    return value


def preparation_token(reply, key, fixture, window, validate_geometry):
    require(isinstance(reply, dict) and set(reply) == {'ok', 'action_attempted', 'prepared'}
            and reply['ok'] is True and reply['action_attempted'] is False, 'preparation not successful')
    frozen = reply.get('prepared', {})
    require(isinstance(frozen, dict) and set(frozen) == {'token', 'key', 'receiver', 'window', 'expires_in_ms'}
            and frozen.get('key') == key
            and type(frozen.get('expires_in_ms')) is int and frozen['expires_in_ms'] == 10000,
            'wrong preparation key or lifetime')
    token = frozen.get('token')
    require(isinstance(token, str) and len(token) == 42 and token.startswith('macos_key:')
            and all(c in '0123456789abcdef' for c in token[10:]), 'invalid preparation token')
    receiver = frozen.get('receiver', {})
    require(isinstance(receiver, dict) and receiver.get('role') == 'AXTextField'
            and receiver.get('enabled') is True, 'wrong prepared receiver')
    require(frozen.get('window') == {'ax': window, 'cg': window}, 'prepared window differs')
    validate_geometry(receiver, fixture, window)
    return token


def definite_no_post(reply):
    """Only explicit zero-effect evidence counts; omissions or partial receipts do not."""
    if not isinstance(reply, dict) or reply.get('ok') is not False:
        return False
    if not (reply.get('action_attempted') is False and reply.get('effects_unconfirmed') is False
            and type(reply.get('posting_calls')) is int and reply['posting_calls'] == 0
            and reply.get('effect_verified') is False
            and (reply.get('focus_interference') is None or reply.get('focus_interference') is False)
            and isinstance(reply.get('error'), str) and reply['error']):
        return False
    return 'action' not in reply


def public_reply(reply):
    # One-use authority is needed internally but never written into study evidence.
    if not isinstance(reply, dict):
        return {'malformed_reply': True}
    result = dict(reply)
    if isinstance(result.get('prepared'), dict):
        result['prepared'] = {k: v for k, v in result['prepared'].items() if k != 'token'}
    return result


def public_fixture(state):
    # Unexpected input in even our disposable window must not copy arbitrary text.
    if not isinstance(state, dict):
        return {'malformed_fixture': True}
    out = {k: state[k] for k in ('active', 'overflow', 'count', 'caret', 'end') if k in state}
    out['value'] = 'fixture-a' if state.get('value') == 'fixture-a' else '[changed]'
    events = state.get('events')
    if isinstance(events, list):
        out['events'] = []
        for event in events[:8]:
            if not isinstance(event, dict):
                out['events'].append({'unexpected_event': True}); continue
            saved = {k: event[k] for k in ('kind', 'target', 'trusted', 'repeat', 'alt', 'control', 'meta', 'shift') if k in event}
            for key in ('key', 'code'):
                saved[key] = event.get(key) if event.get(key) in ('ArrowLeft', 'ArrowRight') else '[unexpected]'
            out['events'].append(saved)
    return out


def summarize(report):
    dispatch = report.get('dispatch', {})
    native = dispatch.get('native_after', {}) if dispatch.get('witness_valid') is True else {}
    activity = native.get('hid_activity', {})
    deltas = activity.get('deltas', {})
    keyboard = None if not deltas or activity.get('counter_regression') else bool(deltas['key_down'] or deltas['key_up'])
    return {'outcome': report.get('outcome', 'incomplete'),
            'preparation_attempted': report.get('preparation', {}).get('attempted', False),
            'dispatch_request_attempted': dispatch.get('attempted', False),
            'effect_verified': report.get('assessment', {}).get('effect_verified', False),
            'hid_activity_during_dispatch_bracket': activity.get('changed'),
            'hid_keyboard_activity_during_dispatch_bracket': keyboard,
            'human_focus_changed_during_dispatch_bracket': native.get('human_changed'),
            'human_activity_attribution': 'not_authenticated',
            'internal_posting_overlap_verified': False,
            'continuous_isolation_verified': False}


def collect(call, evaluate, binding, witness, validate_geometry, window, report,
            checkpoint, deadline, key, clock=time.monotonic, pause=time.sleep):
    require(key in ('ArrowLeft', 'ArrowRight') and math.isfinite(deadline), 'bounded fixed key required')
    report.update(completed=False, measurement_valid=False, key=key,
                  keyboard_input_requested=True, automatic_input_retry=False,
                  human_focus_mutated=False)
    checkpoint()
    state = lambda: evaluate('arrowFixtureState()')
    fixture = lambda: evaluate('keyboardTargetFixtureState()')

    def bracket(name, sequence, operation):
        row = report[name] = {'attempted': False}
        checkpoint()
        require(clock() < deadline, 'key study deadline before witness')
        # Save raw receipts before validating so malformed evidence remains visible.
        row['native_before'] = witness('before', sequence)
        validate_witness(row['native_before'], sequence, 'before')
        checkpoint()
        try:
            require(clock() < deadline, 'key study deadline before operation')
            row['attempted'] = True
            checkpoint()
            started = clock()
            try:
                reply = operation()
                row['reply'] = public_reply(reply)
            finally:
                row['client_elapsed_us'] = max(0, int((clock()-started)*1000000))
        finally:
            try:
                row['native_after'] = witness('after', sequence)
                validate_witness(row['native_after'], sequence, 'after')
                for who in ('human', 'receiver'):
                    require(row['native_after'][who+'_before'] == row['native_before'][who],
                            'native baseline changed')
                row['witness_valid'] = True
            except Exception as error:
                row['witness_finish_error'] = str(error)[:512]
            checkpoint()
        require('witness_finish_error' not in row, 'native witness finish failed')
        require(clock() < deadline, 'key study deadline after operation')
        return reply

    try:
        require(clock() < deadline, 'key study deadline before fixture')
        before = evaluate('armArrowFixture()')
        report['before'] = public_fixture(before)
        require(before == {'active': 'first', 'events': [], 'overflow': False,
                           'count': 0, 'caret': 2, 'end': 2, 'value': 'fixture-a'},
                'disposable key fixture not pristine')
        geometry = fixture()
        require(geometry.get('active') == 'first' and geometry.get('click_overflow') is False
                and isinstance(geometry.get('click_events'), list) and len(geometry['click_events']) == 3,
                'verified setup evidence missing')
        prepared = bracket('preparation', 1, lambda: call('prepare_macos_window_'+key.lower(), binding=binding))
        require(state() == before and fixture() == geometry, 'fixture changed during preparation')
        if isinstance(prepared, dict) and prepared.get('ok') is False:
            require(set(prepared) == {'ok', 'error'}, 'malformed preparation refusal')
            report['refusal'] = study.classify_refusal(prepared['error'])
            report['outcome'] = 'preparation_refused'
        else:
            token = preparation_token(prepared, key, geometry, window, validate_geometry)
            native = bracket('dispatch', 2,
                             lambda: call('press_macos_window_'+key.lower(), binding=binding, token=token))
            # Poll only observations. Never repeat a posting, even after an exception.
            until = min(deadline, clock()+2)
            after = state()
            while len(after.get('events', [])) < 2 and not definite_no_post(native) and clock() < until:
                pause(.05)
                after = state()
            report['after'] = public_fixture(after)
            require(fixture() == geometry, 'fixture geometry/focus/mouse state changed')
            if definite_no_post(native):
                require(after == before, 'refused key changed fixture')
                report['outcome'] = 'dispatch_refused'
                report['refusal'] = study.classify_refusal(native['error'])
            else:
                assessment = arrows.verify_effect(native, before, after, key)
                require(native.get('action_attempted') is True and native.get('effects_unconfirmed') is True
                        and native.get('focus_interference') is False, 'outer dispatch receipt disagrees')
                action = native['action']
                frozen = prepared['prepared']
                require(action.get('receiver') == action.get('after_receiver') == frozen['receiver']
                        and action.get('before') == action.get('after') == frozen['window'],
                        'native key receipt differs from exact preparation')
                report['assessment'] = assessment
                report['outcome'] = 'effect_verified'
        require(clock() < deadline, 'key study deadline at completion')
        report.update(completed=True, measurement_valid=True)
    except Exception as error:
        report['stop_reason'] = str(error)[:8192]
        # Preserve available fixture effects even if posting lost its reply or witness.
        if report.get('dispatch', {}).get('attempted'):
            try:
                report['after'] = public_fixture(state())
            except Exception:
                report['effect_observation_unavailable'] = True
        raise
    finally:
        report['summary'] = summarize(report)
        checkpoint()
