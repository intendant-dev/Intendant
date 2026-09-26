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


def validate_current_activity(value, sequence):
    require(isinstance(value, dict) and set(value) == {'sequence', 'hid_activity'}
            and type(value.get('sequence')) is int and value['sequence'] == sequence,
            'current activity sequence/schema')
    activity = value['hid_activity']
    require(isinstance(activity, dict) and set(activity) == {
        'source', 'counter_regression', 'deltas', 'changed', 'attribution'
    } and activity.get('source') == 'hid_system'
      and activity.get('attribution') == 'not_authenticated'
      and type(activity.get('counter_regression')) is bool,
      'current activity schema')
    deltas = activity['deltas']
    require(isinstance(deltas, dict) and set(deltas) == set(COUNTERS),
            'current activity counters')
    if activity['counter_regression']:
        require(activity['changed'] is None and all(v is None for v in deltas.values()),
                'regressed current counters must be unknown')
    else:
        require(all(type(v) is int and 0 <= v <= 2**32-1 for v in deltas.values()),
                'current activity delta type/range')
        require(type(activity['changed']) is bool and activity['changed'] == any(deltas.values()),
                'current activity change disagrees with counters')
    return value


def summarize_client_activity(before, samples):
    before = validate_current_activity(
        before, before.get('sequence') if isinstance(before, dict) else -1)
    require(not before['hid_activity']['counter_regression'],
            'client activity baseline counter regression')
    require(isinstance(samples, list) and samples,
            'no activity samples while client dispatch was alive')
    sequence = before['sequence']
    checked = [validate_current_activity(value, sequence) for value in samples]
    require(all(not value['hid_activity']['counter_regression'] for value in checked),
            'activity counter regression while client dispatch was alive')
    base = before['hid_activity']['deltas']
    progress = {}
    for counter in COUNTERS:
        peak = max(value['hid_activity']['deltas'][counter] for value in checked)
        require(peak >= base[counter],
                'activity counter moved backwards while client dispatch was alive')
        progress[counter] = peak - base[counter]
    return {
        'sample_count': len(checked),
        'deltas_while_client_alive': progress,
        'changed': any(progress.values()),
        'keyboard_activity': bool(progress['key_down'] or progress['key_up']),
        'attribution': 'not_authenticated',
    }


def validate_mouse_overlap(before, samples):
    before = validate_current_activity(
        before, before.get('sequence') if isinstance(before, dict) else -1)
    require(not before['hid_activity']['counter_regression'],
            'mouse overlap baseline counter regression')
    base = before['hid_activity']['deltas']
    require(base['mouse_move'] > 0, 'required mouse activity was not observed before dispatch')
    require(base['key_down'] == base['key_up'] == 0,
            'keyboard HID activity observed before mouse-only dispatch')
    client = summarize_client_activity(before, samples)
    require(not client['keyboard_activity'],
            'keyboard HID activity observed in mouse-only overlap profile')
    mouse_progress = client['deltas_while_client_alive']['mouse_move']
    require(mouse_progress > 0,
            'mouse activity did not progress while client dispatch process was alive')
    return {
        'before_dispatch': base['mouse_move'],
        'while_client_alive': mouse_progress,
        'attribution': 'not_authenticated',
        'client_process_overlap_verified': True,
        'internal_posting_overlap_verified': False,
    }


def summarize_keyboard_overlap(before, samples):
    before = validate_current_activity(
        before, before.get('sequence') if isinstance(before, dict) else -1)
    require(not before['hid_activity']['counter_regression'],
            'keyboard overlap baseline counter regression')
    base = before['hid_activity']['deltas']
    require(base['key_down'] > 0 and base['key_up'] > 0,
            'required completed keyboard activity not observed before dispatch')
    client = summarize_client_activity(before, samples)
    # Only differences between two samples both taken while the client lives
    # establish in-flight progress; the prelaunch gate is not that baseline.
    client = summarize_client_activity(samples[0], samples)
    for previous, current in zip([before] + samples, samples):
        require(all(current['hid_activity']['deltas'][k] >= previous['hid_activity']['deltas'][k]
                    for k in COUNTERS), 'keyboard counter sample moved backwards')
    progress = client['deltas_while_client_alive']
    return {
        'before_dispatch': {
            'key_down': base['key_down'],
            'key_up': base['key_up'],
        },
        'while_client_alive': {
            'key_down': progress['key_down'],
            'key_up': progress['key_up'],
        },
        'progress_exceeds_single_tested_pair':
            progress['key_down'] >= 2 and progress['key_up'] >= 2,
        'attribution': 'not_authenticated',
        'client_samples_collected': True,
        'client_process_overlap_verified': False,
        'internal_posting_overlap_verified': False,
    }


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


def finalize_keyboard_overlap(outcome, dispatch_reply, overlap, native_after):
    """Keep prior activity, in-flight progress, refusal, and delivery distinct."""
    require(isinstance(overlap, dict) and set(overlap) == {
        'before_dispatch', 'while_client_alive', 'progress_exceeds_single_tested_pair',
        'client_samples_collected', 'attribution', 'client_process_overlap_verified',
        'internal_posting_overlap_verified'}, 'required keyboard overlap evidence missing')
    for phase in ('before_dispatch', 'while_client_alive'):
        counts = overlap[phase]
        require(isinstance(counts, dict) and set(counts) == {'key_down', 'key_up'}
                and all(type(v) is int and 0 <= v <= 2**32 - 1 for v in counts.values()),
                'invalid keyboard overlap counters')
    before, during = overlap['before_dispatch'], overlap['while_client_alive']
    extra_pair = during['key_down'] >= 2 and during['key_up'] >= 2
    require(all(v > 0 for v in before.values())
            and overlap['client_samples_collected'] is True
            and overlap['attribution'] == 'not_authenticated'
            and overlap['client_process_overlap_verified'] is False
            and overlap['internal_posting_overlap_verified'] is False
            and overlap['progress_exceeds_single_tested_pair'] is extra_pair,
            'inconsistent keyboard overlap evidence')
    validate_witness(native_after, 2, 'after')
    result = dict(overlap)
    if outcome == 'dispatch_refused':
        require(definite_no_post(dispatch_reply),
                'keyboard overlap refusal lacks explicit zero-post evidence')
        result['containment_outcome'] = 'dispatch_refused_zero_post'
        result['client_process_overlap_verified'] = all(v > 0 for v in during.values())
        return result
    if outcome == 'effect_verified':
        require(extra_pair, 'keyboard HID counter progress did not exceed the single tested arrow pair')
        require(all(native_after.get(k) is False for k in
                    ('human_changed', 'receiver_changed', 'foreground_changed', 'target_foreground_observed')),
                'keyboard containment requires known unchanged focus endpoints')
        result['containment_outcome'] = 'effect_verified_with_keyboard_overlap'
        result['client_process_overlap_verified'] = True
        return result
    raise RuntimeError('keyboard overlap study reached unsupported outcome: ' + str(outcome))


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
    overlap = report.get('mouse_overlap')
    keyboard_overlap = report.get('keyboard_overlap')
    client = report.get('dispatch_client_activity')
    native = dispatch.get('native_after', {}) if dispatch.get('witness_valid') is True else {}
    activity = native.get('hid_activity', {})
    deltas = activity.get('deltas', {})
    keyboard = None if not deltas or activity.get('counter_regression') else bool(
        deltas['key_down'] or deltas['key_up'])
    return {'outcome': report.get('outcome', 'incomplete'),
            'preparation_attempted': report.get('preparation', {}).get('attempted', False),
            'dispatch_request_attempted': dispatch.get('attempted', False),
            'effect_verified': report.get('assessment', {}).get('effect_verified', False),
            'hid_activity_during_dispatch_witness_bracket': activity.get('changed'),
            'hid_keyboard_activity_during_dispatch_witness_bracket': keyboard,
            'human_focus_changed_during_dispatch_witness_bracket': native.get('human_changed'),
            'hid_activity_while_dispatch_client_alive':
                client.get('changed') if isinstance(client, dict) else None,
            'hid_keyboard_activity_while_dispatch_client_alive':
                client.get('keyboard_activity') if isinstance(client, dict) else None,
            'human_activity_attribution': 'not_authenticated',
            'mouse_activity_required': report.get('mouse_activity_required', False),
            'keyboard_activity_required': report.get('keyboard_activity_required', False),
            'mouse_activity_before_dispatch': overlap.get('before_dispatch') if isinstance(overlap, dict) else None,
            'mouse_activity_while_client_alive': overlap.get('while_client_alive') if isinstance(overlap, dict) else None,
            'keyboard_activity_before_dispatch':
                keyboard_overlap.get('before_dispatch') if isinstance(keyboard_overlap, dict) else None,
            'keyboard_activity_while_client_alive':
                keyboard_overlap.get('while_client_alive') if isinstance(keyboard_overlap, dict) else None,
            'keyboard_progress_exceeds_tested_pair':
                keyboard_overlap.get('progress_exceeds_single_tested_pair') if isinstance(keyboard_overlap, dict) else None,
            'keyboard_containment_outcome':
                keyboard_overlap.get('containment_outcome') if isinstance(keyboard_overlap, dict) else None,
            'client_process_overlap_verified':
                (overlap.get('client_process_overlap_verified', False) if isinstance(overlap, dict)
                 else keyboard_overlap.get('client_process_overlap_verified', False)
                 if isinstance(keyboard_overlap, dict) else False),
            'internal_posting_overlap_verified': False,
            'continuous_isolation_verified': False}


def collect(call, evaluate, binding, witness, validate_geometry, window, report,
            checkpoint, deadline, key, clock=time.monotonic, pause=time.sleep, before_dispatch=None):
    require(key in ('ArrowLeft', 'ArrowRight') and math.isfinite(deadline), 'bounded fixed key required')
    report.update(completed=False, measurement_valid=False, key=key,
                  keyboard_input_requested=True, automatic_input_retry=False,
                  human_focus_mutated=False)
    checkpoint()
    state = lambda: evaluate('arrowFixtureState()')
    fixture = lambda: evaluate('keyboardTargetFixtureState()')

    def bracket(name, sequence, operation, before_operation=None):
        row = report[name] = {'attempted': False}
        checkpoint()
        require(clock() < deadline, 'key study deadline before witness')
        # Save raw receipts before validating so malformed evidence remains visible.
        row['native_before'] = witness('before', sequence)
        validate_witness(row['native_before'], sequence, 'before')
        checkpoint()
        try:
            # Passive gating precedes the transport attempt, inside the witness bracket.
            if before_operation is not None:
                require(clock() < deadline, 'key study deadline before dispatch gate')
                before_operation()
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
                             lambda: call('press_macos_window_'+key.lower(), binding=binding, token=token),
                             before_operation=before_dispatch)
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
        if 'dispatch' in report:
            try:
                report['after'] = public_fixture(state())
            except Exception:
                report['effect_observation_unavailable'] = True
        raise
    finally:
        report['summary'] = summarize(report)
        checkpoint()
