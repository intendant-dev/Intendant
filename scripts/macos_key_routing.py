"""Observe routing boundaries; a cooperative control is not external key support."""
from macos_key_evidence import integer, key_receipts
from macos_input_evidence import assess_desktop

MODES = {'application': 'self_process_key', 'psn': 'self_process_key_psn',
         'window-control': 'self_process_key_window_control'}


def assess_routing(native, pid, route):
    result = dict(mode=MODES.get(route), diagnostic_valid=False,
        queue_delivery_verified=False, window_delivery_verified=False,
        receiver_delivery_verified=False, control_effect_verified=False,
        native_background_delivery_verified=False, routing_stop='unknown',
        production_dispatch_enabled=False, cross_process_verified=False)
    control = route == 'window-control'
    if (route not in MODES or not isinstance(native, dict)
            or native.get('mode') != MODES[route]
            or native.get('cooperative_forwarding') is not control
            or not integer(pid, 1, 2**31-1)):
        return result
    plan = native.get('plan')
    if not isinstance(plan, dict) or plan.get('pid') != pid or plan.get('source_pid') != pid:
        return result
    queue = key_receipts(plan, native.get('queue_receipts'))
    window = key_receipts(plan, native.get('window_receipts'))
    view = key_receipts(plan, native.get('receipts'))
    result.update(queue_delivery_verified=queue, window_delivery_verified=window,
                  receiver_delivery_verified=view)
    desktop = assess_desktop(native.get('before'), native.get('after'), pid,
                             native.get('target_ever_front'))
    result['desktop_observation_assessment'] = desktop
    traces = native.get('routing')
    if (not integer(native.get('unrelated_events'), 0, 0)
            or not queue or not isinstance(traces, list) or len(traces) != 2
            or any(not isinstance(t, dict) or t.get('app_active') is not False
                   or not integer(t.get('key_window_id'), 0, 0)
                   or not integer(t.get('main_window_id'), 0, 0)
                   or t.get('event_is_own_window') is not True
                   or t.get('responder_is_view') is not True for t in traces)
            or not integer(native.get('posted_events'), 2, 2)
            or native.get('overflow') is not False
            or native.get('window_closed') is not True
            or native.get('responder_unchanged') is not True
            or native.get('observations_complete') is not True
            or desktop['target_foreground_observed'] is not False
            or any(v is None for v in desktop['sampled_changes'].values())
            or not integer(native.get('effect_count'), 0, 1)):
        return result
    # Malformed/partial receipts cannot establish a clean negative.
    if native.get('window_receipts') != [] and not window:
        return result
    if native.get('receipts') != [] and not view:
        return result
    effect = view and window and native['effect_count'] == 1
    if (view and not window) or (not view and native['effect_count'] != 0):
        return result
    expected_error = '' if effect else 'receiver key delivery/effect not verified; queue receipt is insufficient'
    if native.get('ok') is not effect or native.get('error') != expected_error:
        return result
    if not control and (window or view or effect):
        result['unexpected_receiver_effect'] = True
        return result
    result['diagnostic_valid'] = True
    result['routing_stop'] = 'receiver' if view else 'window_to_responder' if window else 'application_to_window'
    result['control_effect_verified'] = control and effect
    result['native_background_delivery_verified'] = False
    return result
