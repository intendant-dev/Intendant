"""Pure evidence checks for the disposable managed-keyboard page; no IO/input."""
from __future__ import annotations


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def validate_snapshot(value: dict) -> None:
    require(isinstance(value, dict), 'missing fixture snapshot')
    for name in ('first', 'second', 'editor', 'active'):
        require(isinstance(value.get(name), str), f'missing fixture {name}')
    require(value.get('overflow') is False, 'fixture event buffer overflow or unknown')
    require(type(value.get('protectedEvents')) is int and value['protectedEvents'] == 0,
            'protected fixture received input or evidence is unknown')
    require(type(value.get('activations')) is int and value['activations'] >= 0,
            'invalid activation count')
    require(isinstance(value.get('events'), list) and len(value['events']) <= 512,
            'invalid event inventory')


def verify_transition(before: dict, after: dict, expected: dict, *,
                      key: str | None = None, receiver: str | None = None,
                      minimum_input_events: int = 0) -> dict:
    """Readback and actual page events, never a protocol ACK, establish effects.

    This proves a fixture transition only. It makes no claim of continuous OS
    focus isolation or physical-human provenance. InsertText needs input events,
    not fictional physical key events; an explicit key needs its down/up pair.
    """
    validate_snapshot(before)
    validate_snapshot(after)
    require(bool(expected) or key is not None or minimum_input_events > 0, 'no observable effect expectation')
    for name, want in expected.items():
        require(name in ('first', 'second', 'editor', 'active', 'activations', 'firstSelection', 'secondSelection'),
                f'unsupported expected field: {name}')
        require(after[name] == want, f'fixture effect mismatch: {name}')
    require(after['events'][:len(before['events'])] == before['events'],
            'event inventory regressed or earlier evidence changed')
    events = after['events'][len(before['events']):]
    for event in events:
        require(isinstance(event, dict) and event.get('trusted') is True,
                'missing or untrusted page event')
        require(event.get('id') != 'protected', 'protected page event')
        require(event.get('repeat') is False, 'unexpected repeated page event')
    key_events = [e for e in events if e.get('type') in ('keydown', 'keyup')]
    if key is not None:
        require([e.get('type') for e in key_events] == ['keydown', 'keyup'],
                'expected exactly one complete key pair')
        require(all(e.get('key') == key for e in key_events), 'wrong delivered key')
        # Tab can legitimately move the page-local receiver between down/up.
        if receiver is not None:
            require(key_events[0].get('id') == receiver, 'wrong key-down receiver')
    inputs = [e for e in events if e.get('type') == 'input']
    require(len(inputs) >= minimum_input_events, 'missing page input effect')
    return {'effect_verified': True, 'page_key_pairs': len(key_events) // 2,
            'input_events': len(inputs), 'continuous_isolation_verified': False,
            'physical_keyboard_provenance_verified': False}


def verify_witness(witness: dict, browser_pid: int, started_us: int, ended_us: int) -> dict:
    """Require sampled focus/clipboard stability; report activity without attribution."""
    require(witness.get('input_posting_calls') == 0 and witness.get('application_created') is False,
            'observer unexpectedly produced input or an application')
    samples=witness.get('samples')
    require(isinstance(samples,list) and 2 <= len(samples) <= 2420,'missing observer samples')
    require(type(browser_pid) is int and browser_pid>0 and started_us<ended_us,'invalid action interval')
    previous=0
    for sample in samples:
        require(type(sample.get('monotonic_us')) is int and sample['monotonic_us']>previous,
                'observer time missing or regressed')
        previous=sample['monotonic_us']
        for key in ('front_pid','front_started_ms','clipboard_change_count','hid_down','hid_up'):
            require(type(sample.get(key)) is int and sample[key]>=0,'unknown observer state: '+key)
        require(sample['front_pid']>0 and sample['front_pid']!=browser_pid,'target observed foreground')
    preceding=[s for s in samples if s['monotonic_us']<=started_us]
    following=[s for s in samples if s['monotonic_us']>=ended_us]
    require(preceding and following,'observer does not bracket actions')
    first,last=preceding[-1],following[0]
    span=[s for s in samples if first['monotonic_us']<=s['monotonic_us']<=last['monotonic_us']]
    require(all((s['front_pid'],s['front_started_ms']) == (first['front_pid'],first['front_started_ms'])
                for s in span),'foreground context changed during action span')
    require(all(s['clipboard_change_count']==first['clipboard_change_count'] for s in span),
            'clipboard changed during action span; attribution unknown')
    require(all(a[k]<=b[k] for a,b in zip(span,span[1:]) for k in ('hid_down','hid_up')),
            'keyboard counters regressed')
    inner=[s for s in span if started_us<=s['monotonic_us']<=ended_us]
    activity=len(inner)>=2 and (inner[-1]['hid_down']>inner[0]['hid_down'] or inner[-1]['hid_up']>inner[0]['hid_up'])
    return {'sampled_focus_unchanged':True,'sampled_clipboard_unchanged':True,
            'keyboard_activity_observed_in_action_span':activity,'samples_in_action_span':len(inner),
            'human_provenance_verified':False,'continuous_isolation_verified':False}


def verify_request_activity(witness: dict, intervals: list) -> dict:
    """Count activity strictly inside requests, excluding deliberate pacing gaps.

    Call only after verify_witness has validated the complete observer stream.
    A request bracket is not an atomic OS dispatch or proof of human provenance.
    """
    require(isinstance(intervals, list) and 1 <= len(intervals) <= 32,
            'invalid keyboard request intervals')
    samples = witness['samples']
    results = []
    for index, (start, end) in enumerate(intervals):
        require(type(start) is int and type(end) is int and 0 < start < end,
                'invalid keyboard request interval')
        inner = [sample for sample in samples if start <= sample['monotonic_us'] <= end]
        observed = len(inner) >= 2 and any(
            inner[-1][key] > inner[0][key] for key in ('hid_down', 'hid_up'))
        results.append({'index': index, 'samples': len(inner),
                        'keyboard_activity_observed': observed})
    return {'keyboard_activity_observed_during_request':
            any(result['keyboard_activity_observed'] for result in results),
            'requests': results, 'human_provenance_verified': False,
            'continuous_isolation_verified': False}
