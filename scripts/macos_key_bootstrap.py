"""Pure click-before-key fixture planning and evidence checks.

This module neither opens a browser nor posts input.  It keeps the native
pointer frame deliberately small: a caller may supply observed geometry and a
random correlation tag, but never a PID, key, activation request, or fallback.
"""
import math
import struct


MAX_POINT = 1_000_000
MAX_TAG = 2**63 - 1
METRIC_KEYS = ('width', 'height', 'outer_width', 'outer_height', 'screen_x',
               'screen_y', 'scroll_x', 'scroll_y', 'scale', 'device_scale_factor')
RECT_KEYS = ('x', 'y', 'width', 'height')
BOUND_KEYS = ('X', 'Y', 'Width', 'Height')


def require(value, detail):
    if not value:
        raise RuntimeError(str(detail))


def number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value) and abs(value) <= MAX_POINT
    except OverflowError:
        return False


def integer(value, low, high):
    return type(value) is int and low <= value <= high


def _numbers(mapping, keys, detail):
    require(isinstance(mapping, dict) and all(number(mapping.get(key)) for key in keys), detail)
    return {key: mapping[key] for key in keys}


def _metrics(metrics):
    value = _numbers(metrics, METRIC_KEYS, 'invalid browser metrics')
    require(value['device_scale_factor'] > 0, 'invalid backing scale')
    require(value['scale'] == 1 and value['scroll_x'] == value['scroll_y'] == 0,
            'zoom or scroll changed')
    require(value['width'] >= 200 and value['height'] >= 200
            and value['outer_width'] >= value['width']
            and value['outer_height'] >= value['height'], 'invalid browser viewport')
    require(value['outer_width'] == value['width'], 'unsupported horizontal browser border')
    top = value['outer_height'] - value['height']
    require(0 <= top <= 200, 'invalid browser chrome height')
    return value


def _bounds(bounds):
    value = _numbers(bounds, BOUND_KEYS, 'invalid native bounds')
    require(200 <= value['Width'] <= 16384 and 200 <= value['Height'] <= 16384,
            'invalid native dimensions')
    return value


def _receiver(rect):
    value = _numbers(rect, RECT_KEYS, 'invalid receiver rectangle')
    require(value['width'] >= 40 and value['height'] >= 40, 'receiver too small')
    return value


def _same_layout(metrics, bounds):
    return all(abs(metrics[left] - bounds[right]) <= 1 for left, right in
               (('screen_x', 'X'), ('screen_y', 'Y'),
                ('outer_width', 'Width'), ('outer_height', 'Height')))


def _inside(rect, x, y):
    return (rect['x'] + 20 <= x < rect['x'] + rect['width'] - 20
            and rect['y'] + 20 <= y < rect['y'] + rect['height'] - 20)


def freeze_click_tag(key_tag, random_candidate):
    """Keep a fresh random click tag distinct without an input retry."""
    require(integer(key_tag, 1, MAX_TAG) and integer(random_candidate, 1, MAX_TAG),
            'invalid correlation tag')
    return random_candidate if random_candidate != key_tag else (random_candidate % MAX_TAG) + 1


def build_click_plan(receiver, metrics, bounds, window_id, tag):
    """Build the one native click plan from the observed receiver and window."""
    rect = _receiver(receiver)
    snapshot = _metrics(metrics)
    native_bounds = _bounds(bounds)
    require(integer(window_id, 1, 2**32 - 1) and integer(tag, 1, MAX_TAG),
            'invalid click identity')
    require(_same_layout(snapshot, native_bounds), 'browser/native geometry mismatch')
    client_x = round(rect['x'] + rect['width'] * .37)
    client_y = round(rect['y'] + rect['height'] * .61)
    require(_inside(rect, client_x, client_y)
            and 0 <= client_x < snapshot['width'] and 0 <= client_y < snapshot['height'],
            'click outside receiver or viewport')
    top = snapshot['outer_height'] - snapshot['height']
    screen_x = native_bounds['X'] + client_x
    screen_y = native_bounds['Y'] + top + client_y
    local_x = screen_x - native_bounds['X']
    local_y = screen_y - native_bounds['Y']
    require(24 <= local_x <= native_bounds['Width'] - 24
            and 24 <= local_y <= native_bounds['Height'] - 24,
            'click outside native window interior')
    return {'window_id': window_id, 'tag': tag, 'bounds': native_bounds,
            'receiver': rect, 'metrics': snapshot, 'client_x': client_x,
            'client_y': client_y, 'screen_x': screen_x, 'screen_y': screen_y,
            'local_x': local_x, 'local_y': local_y}


def valid_click_plan(plan):
    try:
        require(isinstance(plan, dict), 'invalid click plan')
        rect = _receiver(plan.get('receiver'))
        snapshot = _metrics(plan.get('metrics'))
        bounds = _bounds(plan.get('bounds'))
        require(integer(plan.get('window_id'), 1, 2**32 - 1)
                and integer(plan.get('tag'), 1, MAX_TAG), 'invalid click identity')
        require(_same_layout(snapshot, bounds), 'browser/native geometry mismatch')
        for key in ('client_x', 'client_y', 'screen_x', 'screen_y', 'local_x', 'local_y'):
            require(number(plan.get(key)), 'invalid click coordinate')
        require(_inside(rect, plan['client_x'], plan['client_y'])
                and 0 <= plan['client_x'] < snapshot['width']
                and 0 <= plan['client_y'] < snapshot['height'], 'click outside receiver')
        top = snapshot['outer_height'] - snapshot['height']
        require(plan['screen_x'] == bounds['X'] + plan['client_x']
                and plan['screen_y'] == bounds['Y'] + top + plan['client_y']
                and plan['local_x'] == plan['screen_x'] - bounds['X']
                and plan['local_y'] == plan['screen_y'] - bounds['Y'],
                'inconsistent click coordinates')
        require(24 <= plan['local_x'] <= bounds['Width'] - 24
                and 24 <= plan['local_y'] <= bounds['Height'] - 24,
                'click outside native window interior')
    except RuntimeError:
        return False
    return True


def encode_click_plan(plan):
    require(valid_click_plan(plan), 'invalid click plan')
    bounds = plan['bounds']
    return b'p' + struct.pack('<II8dq', 1, plan['window_id'], bounds['X'], bounds['Y'],
                               bounds['Width'], bounds['Height'], plan['screen_x'],
                               plan['screen_y'], plan['local_x'], plan['local_y'], plan['tag'])


def native_click_evidence(plan):
    """The immutable subset the native owner retains through shutdown."""
    require(valid_click_plan(plan), 'invalid click plan')
    return {'window_id': plan['window_id'], 'tag': plan['tag'], 'bounds': dict(plan['bounds']),
            'screen_x': plan['screen_x'], 'screen_y': plan['screen_y'],
            'local_x': plan['local_x'], 'local_y': plan['local_y']}


def assess_click(plan, native, state, nonce, receiver, sender, key_tag):
    """Require one exact trusted DOM click before the key pair may be sent."""
    result = {'passed': False, 'effect_verified': False, 'layout_unchanged': False,
              'dom_tag_correlation': False, 'continuous_isolation_verified': False}
    if (not valid_click_plan(plan) or not isinstance(native, dict) or not isinstance(state, dict)
            or not integer(receiver, 1, 2**31 - 1) or not integer(sender, 1, 2**31 - 1)
            or receiver == sender or not integer(key_tag, 1, MAX_TAG)
            or plan['tag'] == key_tag or not isinstance(nonce, str) or len(nonce) != 32
            or any(char not in '0123456789abcdef' for char in nonce)):
        return result
    if (native.get('dispatch_attempted') is not True or native.get('effect_verified') is not False
            or type(native.get('posted_events')) is not int or native['posted_events'] != 2
            or 'error' in native
            or any(type(native.get(key)) is not int or native[key] != expected
                   for key, expected in (('source_pid', sender), ('target_pid', receiver),
                                         ('window_id', plan['window_id']), ('tag', plan['tag'])))):
        return result
    events = state.get('click_events')
    if (state.get('nonce') != nonce or state.get('active') is not True
            or state.get('click_overflow') is not False
            or type(state.get('click_unrelated')) is not int or state['click_unrelated'] != 0
            or type(state.get('count')) is not int or state['count'] != 0
            or state.get('events') != [] or not isinstance(events, list) or len(events) != 1):
        return result
    try:
        layout = _metrics(state.get('metrics'))
        receiver_rect = _receiver(state.get('receiver_rect'))
    except RuntimeError:
        return result
    if layout != plan['metrics'] or receiver_rect != plan['receiver']:
        return result
    result['layout_unchanged'] = True
    event = events[0]
    if not isinstance(event, dict) or event.get('type') != 'click' or event.get('trusted') is not True:
        return result
    if (type(event.get('button')) is not int or event['button'] != 0
            or type(event.get('buttons')) is not int or event['buttons'] != 0
            or any(event.get(key) is not False for key in ('shift', 'ctrl', 'alt', 'meta'))):
        return result
    for observed, expected in (('client_x', 'client_x'), ('client_y', 'client_y'),
                               ('screen_x', 'screen_x'), ('screen_y', 'screen_y')):
        if not number(event.get(observed)) or event[observed] != plan[expected]:
            return result
    result.update(passed=True, effect_verified=True)
    return result


def encode_click_receipt(plan, native, state, nonce, receiver, sender, key_tag, challenge):
    """Exact-plan DOM attestation for one owner challenge, not authenticated proof."""
    require(integer(challenge, 1, MAX_TAG), 'invalid native click challenge')
    require(assess_click(plan, native, state, nonce, receiver, sender, key_tag)['passed'],
            'DOM click receipt must verify before acknowledgement')
    return b'a' + encode_click_plan(plan)[1:] + struct.pack(
        '<qQII4d', key_tag, challenge, 1, 1, plan['client_x'], plan['client_y'],
        plan['metrics']['width'], plan['metrics']['height'])
