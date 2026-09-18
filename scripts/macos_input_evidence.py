"""Evidence, not authority: desktop samples and tagged synthetic fixture input.

A correlation tag is not a security credential and cannot prove isolation.
"""
import math


def _number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value)
    except OverflowError:
        return False


def _pid(value):
    return type(value) is int and 0 < value < 2**31


def assess_desktop(before, after, target_pid, target_ever_front=None):
    """Compare available samples without attributing human activity to the agent."""
    before = before if isinstance(before, dict) else {}
    after = after if isinstance(after, dict) else {}
    changes = {}
    for channel, keys in (
        ('foreground_application', ('front_pid',)),
        ('pointer', ('pointer_x', 'pointer_y')),
        ('clipboard_change_count', ('clipboard_change_count',)),
    ):
        def valid(sample):
            if channel == 'foreground_application':
                return _pid(sample.get('front_pid'))
            if channel == 'clipboard_change_count':
                return type(sample.get(keys[0])) is int and sample[keys[0]] >= 0
            return all(_number(sample.get(key)) for key in keys)
        changes[channel] = (any(before[key] != after[key] for key in keys)
                            if valid(before) and valid(after) else None)
    sampled_target = (_pid(target_pid) and any(
        _pid(sample.get('front_pid')) and sample['front_pid'] == target_pid
        for sample in (before, after)))
    if sampled_target or target_ever_front is True:
        target_front = True
    elif (_pid(target_pid) and target_ever_front is False
          and all(_pid(sample.get('front_pid')) for sample in (before, after))):
        target_front = False
    else:
        target_front = None
    return {
        'sampled_changes': changes,
        'observation_status': ('sampled_change' if True in changes.values()
                               else 'unavailable' if None in changes.values()
                               else 'no_sampled_change'),
        'change_attribution': 'undetermined',
        'target_foreground_observed': target_front,
        'continuous_isolation_verified': False,
        'action_focus_checks': 'reported_separately',
    }


def assess_pointer_receipts(plan, receipts, tagged_click_count):
    """Require the exact tagged down/up pair plus independent canvas effect.

    Untagged/other-tag events never supply positive evidence for this action.
    The fixture is trusted test instrumentation, not an authorization source.
    """
    result = {'delivery': 'invalid', 'effect_verified': False,
              'matching_receipts': 0, 'unrelated_receipts': 0}
    if (not isinstance(plan, dict) or not isinstance(receipts, list)
            or len(receipts) > 64 or type(tagged_click_count) is not int
            or tagged_click_count < 0):
        return result
    tag = plan.get('tag')
    if (type(tag) is not int or not 0 < tag < 2**63
            or not _pid(plan.get('pid')) or not _pid(plan.get('source_pid'))
            or type(plan.get('window_id')) is not int
            or not 0 < plan['window_id'] < 2**32
            or not all(_number(plan.get(k)) for k in ('x', 'y'))):
        return result
    coordinates = ('x', 'y')
    if any(k in plan for k in ('local_x', 'local_y')):
        if not all(_number(plan.get(k)) for k in ('local_x', 'local_y')):
            return result
        coordinates += ('local_x', 'local_y')
    matching = []
    for receipt in receipts:
        if not isinstance(receipt, dict):
            return result
        if type(receipt.get('tag')) is not int or receipt['tag'] != tag:
            result['unrelated_receipts'] += 1
            continue
        matching.append(receipt)
    result['matching_receipts'] = len(matching)
    for receipt in matching:
        if (any(type(receipt.get(k)) is not int or receipt[k] != plan[k]
                for k in ('pid', 'source_pid', 'window_id'))
                or not all(_number(receipt.get(k)) and
                           abs(receipt[k] - plan[k]) <= 0.5 for k in coordinates)
                or receipt.get('kind') not in ('left_down', 'left_up')):
            return result
    kinds = [receipt['kind'] for receipt in matching]
    result['delivery'] = ('verified' if kinds == ['left_down', 'left_up']
                          else 'not_observed' if not kinds
                          else 'partial_or_duplicate')
    result['effect_verified'] = result['delivery'] == 'verified' and tagged_click_count == 1
    return result
