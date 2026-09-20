"""Pure evidence checks for one owner-bound scroll; no native or browser input.

Native posting is a report, not proof of an application effect. Only the
independent wheel record AND the pane's observed offset can verify that effect.
The DOM cannot observe the native source tag or prove continuous isolation.
"""
import argparse
import math
import re


INITIAL_SCROLL_TOP = 700
MAX_SCROLL_DELTA = 600


def valid_scroll_delta(value):
    return type(value) is int and value != 0 and -MAX_SCROLL_DELTA <= value <= MAX_SCROLL_DELTA


def parse_scroll_delta(value):
    """CLI spelling is decimal, optionally signed; never coerce floats/bools."""
    if isinstance(value, str) and re.fullmatch(r'[+-]?[0-9]+', value):
        try:
            delta = int(value, 10)
            if valid_scroll_delta(delta):
                return delta
        except ValueError:
            pass  # Includes Python's oversized integer-string refusal.
    raise argparse.ArgumentTypeError('scroll delta must be a nonzero signed integer within -600..600')


def _number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value) and abs(value) <= 1_000_000
    except OverflowError:
        return False


def matches_window_observation(observation, bounds):
    """Both native geometry observations must match the independently read window."""
    if not isinstance(observation, dict) or not isinstance(bounds, dict):
        return False
    for native, independent in (('x', 'X'), ('y', 'Y'), ('width', 'Width'), ('height', 'Height')):
        if not _number(bounds.get(independent)):
            return False
        for source in ('ax', 'cg'):
            rectangle = observation.get(source)
            if (not isinstance(rectangle, dict) or not _number(rectangle.get(native))
                    or abs(rectangle[native] - bounds[independent]) > 1):
                return False
        if abs(observation['ax'][native] - observation['cg'][native]) > 1:
            return False
    return 200 <= bounds['Width'] <= 16384 and 200 <= bounds['Height'] <= 16384


def certain_refusal(receipt, *, dispatch=True):
    """Uncertain/partial action receipts never satisfy a negative input case.

Preparation is nonposting and its validation errors may omit action_attempted.
Dispatch must explicitly report false; missing/null is not a refusal proof.
"""
    return (isinstance(receipt, dict) and receipt.get('ok') is False
            and receipt.get('action_attempted', None if dispatch else False) is False
            and receipt.get('effects_unconfirmed', False) is False
            and receipt.get('action') is None)


def assess_scroll(plan, native, state, nonce, delta_y, initial_scroll_top=INITIAL_SCROLL_TOP):
    """Assess a plan from plan_pointer against a receipt and independent DOM state."""
    result = {'passed': False, 'effect_verified': False, 'native_dispatch_reported': False,
              'wheel_observed': False, 'scroll_offset_verified': False,
              'dom_tag_correlation': False, 'continuous_isolation_verified': False}

    def refuse(reason):
        result['reason'] = reason
        return result

    if not valid_scroll_delta(delta_y):
        return refuse('invalid requested scroll delta')
    if not _number(initial_scroll_top) or initial_scroll_top != INITIAL_SCROLL_TOP:
        return refuse('initial offset must be 700, configured before arming')
    if not all(isinstance(value, dict) for value in (plan, native, state)):
        return refuse('missing plan, receipt or DOM state')
    action = native.get('action')
    if (native.get('ok') is not True or native.get('error') is not None
            or native.get('effect_verified', False) is not False
            or native.get('action_attempted') is not True
            or native.get('effects_unconfirmed') is not True
            or native.get('focus_interference') is not False
            or not isinstance(action, dict) or action.get('status') != 'dispatched'
            or action.get('effect_verified') is not False
            or action.get('action_attempted') is not True or action.get('effects_unconfirmed') is not True
            or action.get('focus_interference') is not False
            or 'detail' not in action or action['detail'] is not None
            or type(action.get('posting_calls')) is not int or action['posting_calls'] != 1
            or not valid_scroll_delta(action.get('delta_y')) or action['delta_y'] != delta_y):
        return refuse('native scroll receipt is failed, incomplete, uncertain or mismatched')
    for field, keys in (('point', ('local_x', 'local_y')), ('global', ('x', 'y'))):
        point = action.get(field)
        if (not isinstance(point, dict) or any(not _number(point.get(a)) or not _number(plan.get(b))
                or abs(point[a] - plan[b]) > .001 for a, b in zip(('x', 'y'), keys))):
            return refuse('native point does not match the planned point')
    if not all(matches_window_observation(action.get(field), plan.get('bounds'))
               for field in ('before', 'after')):
        return refuse('native before/after window geometry is unavailable or changed')
    # Both independently match the plan; also require no before/after drift > 1.
    for source in ('ax', 'cg'):
        if any(abs(action['before'][source][key] - action['after'][source][key]) > 1
               for key in ('x', 'y', 'width', 'height')):
            return refuse('native window changed during posting')
    result['native_dispatch_reported'] = True
    events = state.get('events')
    if (not isinstance(nonce, str) or re.fullmatch(r'[a-f0-9]{32}', nonce) is None
            or state.get('nonce') != nonce or state.get('ready') != 'complete'
            or type(state.get('wheels')) is not int or state['wheels'] != 1
            or state.get('overflow') is not False or type(state.get('unrelated')) is not int
            or state['unrelated'] != 0 or not isinstance(events, list) or len(events) != 1):
        return refuse('DOM nonce, readiness or exact event count did not match')
    event = events[0]
    if (not isinstance(event, dict) or event.get('type') != 'wheel' or event.get('trusted') is not True
            or type(event.get('delta_mode')) is not int or event['delta_mode'] != 0
            or not _number(event.get('delta_x')) or event['delta_x'] != 0
            or not _number(event.get('delta_y')) or event['delta_y'] != delta_y
            or any(event.get(key) is not False for key in ('alt_key', 'ctrl_key', 'meta_key', 'shift_key'))):
        return refuse('DOM wheel was untrusted, modified or had the wrong delta')
    for key, planned in (('client_x', 'client_x'), ('client_y', 'client_y'), ('screen_x', 'x'), ('screen_y', 'y')):
        if not _number(event.get(key)) or not _number(plan.get(planned)) or abs(event[key] - plan[planned]) > 1:
            return refuse('DOM wheel coordinates did not match the planned point')
    result['wheel_observed'] = True
    if (not _number(state.get('scroll_top')) or state['scroll_top'] != initial_scroll_top + delta_y
            or not _number(state.get('scroll_left')) or state['scroll_left'] != 0):
        return refuse('observed pane offset did not equal 700 + requested delta with scrollLeft 0')
    result.update(passed=True, effect_verified=True, scroll_offset_verified=True)
    return result
