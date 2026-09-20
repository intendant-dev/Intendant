"""Fixture evidence only. Never authority or proof of an independent input seat."""

def integer(value, low, high):
    return type(value) is int and low <= value <= high


def assess_browser(plan, native, state, nonce, receiver, sender):
    result = {'passed': False, 'effect_verified': False, 'dom_tag_correlation': False,
              'continuous_isolation_verified': False}
    if not all(isinstance(x, dict) for x in (plan, native, state)):
        return result
    if (not integer(receiver, 1, 2**31-1) or not integer(sender, 1, 2**31-1)
            or sender == receiver or not integer(plan.get('window_id'), 1, 2**32-1)
            or not integer(plan.get('tag'), 1, 2**63-1)
            or not isinstance(nonce, str) or len(nonce) != 32
            or any(c not in '0123456789abcdef' for c in nonce)):
        return result
    if (native.get('dispatch_attempted') is not True or native.get('effect_verified') is not False
            or not integer(native.get('posted_events'), 2, 2) or native.get('error')
            or any(type(native.get(k)) is not int or native[k] != expected for k, expected in
                [('source_pid', sender), ('target_pid', receiver), ('keycode', 124),
                 ('window_id', plan['window_id']), ('tag', plan['tag'])])):
        return result
    events = state.get('events')
    if (state.get('nonce') != nonce or not integer(state.get('count'), 1, 1)
            or state.get('active') is not True or state.get('overflow') is not False
            or not integer(state.get('unrelated'), 0, 0) or not isinstance(events, list)
            or len(events) != 2):
        return result
    for event, kind in zip(events, ('keydown', 'keyup')):
        if (not isinstance(event, dict) or event.get('type') != kind
                or event.get('trusted') is not True or event.get('key') != 'ArrowRight'
                or event.get('code') != 'ArrowRight' or event.get('target') != 'receiver'
                or any(event.get(k) is not False for k in ('repeat', 'shift', 'ctrl', 'alt', 'meta'))):
            return result
    result.update(passed=True, effect_verified=True)
    return result


def key_receipts(plan, receipts):
    if (not isinstance(plan, dict) or not isinstance(receipts, list) or len(receipts) != 2
            or any(not integer(plan.get(k), 1, cap) for k, cap in
                [('pid', 2**31-1), ('source_pid', 2**31-1), ('window_id', 2**32-1), ('tag', 2**63-1)])
            or not integer(plan.get('keycode'), 124, 124)):
        return False
    for event, kind in zip(receipts, ('key_down', 'key_up')):
        if (not isinstance(event, dict) or event.get('kind') != kind
                or any(type(event.get(k)) is not int or event[k] != plan[k]
                       for k in ('pid', 'source_pid', 'window_id', 'tag', 'keycode'))
                or not integer(event.get('flags'), 1 << 23, 1 << 23)
                or event.get('repeat') is not False or event.get('expected_character') is not True):
            return False
    return True


def assess_self(native, supervised_pid):
    from macos_input_evidence import assess_desktop
    result = {'passed': False, 'queue_delivery_verified': False,
              'receiver_delivery_verified': False, 'effect_verified': False,
              'production_dispatch_enabled': False, 'cross_process_verified': False}
    if (not isinstance(native, dict) or native.get('mode') != 'self_process_key'
            or native.get('cooperative_forwarding', False) is not False):
        return result
    plan = native.get('plan')
    if (not isinstance(plan, dict) or not integer(supervised_pid, 1, 2**31-1)
            or plan.get('pid') != supervised_pid or plan.get('source_pid') != supervised_pid):
        return result
    result['queue_delivery_verified'] = key_receipts(plan, native.get('queue_receipts'))
    result['receiver_delivery_verified'] = key_receipts(plan, native.get('receipts'))
    result['effect_verified'] = (result['receiver_delivery_verified']
                                 and integer(native.get('effect_count'), 1, 1))
    desktop = assess_desktop(native.get('before'), native.get('after'),
                             supervised_pid, native.get('target_ever_front'))
    result['desktop_observation_assessment'] = desktop
    result['passed'] = (native.get('ok') is True and not native.get('error')
        and result['queue_delivery_verified'] and result['effect_verified']
        and integer(native.get('posted_events'), 2, 2) and native.get('overflow') is False
        and native.get('window_closed') is True and native.get('responder_unchanged') is True
        and native.get('observations_complete') is True
        and desktop['target_foreground_observed'] is False
        and all(v is not None for v in desktop['sampled_changes'].values()))
    return result
