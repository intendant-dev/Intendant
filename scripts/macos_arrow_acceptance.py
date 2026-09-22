"""Disposable-only ArrowRight effect checks. Never inject through CDP."""
import time

def require(value, detail):
    if not value:
        raise RuntimeError(str(detail))

def verify_effect(native, before, after):
    require(isinstance(native,dict) and native.get('ok') is True, 'ArrowRight dispatch refused or uncertain')
    action=native.get('action',{})
    require(action.get('key')=='ArrowRight' and action.get('status')=='dispatched'
        and type(action.get('posting_calls')) is int and action['posting_calls']==2
        and action.get('effect_verified') is False and action.get('effects_unconfirmed') is True
        and action.get('action_attempted') is True and action.get('focus_interference') is False
        and action.get('receiver_unchanged') is True and action.get('detail') is None, 'invalid native key receipt')
    require(before.get('active')=='first' and before.get('events')==[]
        and before.get('caret')==before.get('end')==2 and before.get('value')=='fixture-a', 'fixture not pristine')
    require(after.get('active')=='first' and after.get('value')==before['value']
        and type(after.get('count')) is int and after['count']==1 and after.get('overflow') is False
        and type(after.get('caret')) is int and type(after.get('end')) is int
        and after['caret']==after['end']==3, 'ArrowRight did not move the exact caret once')
    events=after.get('events')
    require(isinstance(events,list) and len(events)==2, 'not exactly one key pair')
    for event,kind in zip(events,('keydown','keyup')):
        require(isinstance(event,dict) and event.get('kind')==kind and event.get('target')=='first'
            and event.get('key')=='ArrowRight' and event.get('code')=='ArrowRight'
            and event.get('trusted') is True and event.get('repeat') is False
            and all(event.get(k) is False for k in ('alt','control','meta','shift')), 'wrong receiver or key event')
    return {'effect_verified':True,'caret_moved':True,'dom_tag_correlation':False,'continuous_isolation_verified':False}

def exercise(call,evaluate,binding,report):
    """Called only after separately authorized and verified fixture setup click."""
    checks=report.setdefault('checks',{})
    def state(): return evaluate('arrowFixtureState()')
    def prepare():
        p=call('prepare_macos_window_arrowright',binding=binding)
        report['last_preparation']=p
        require(p.get('ok') is True,p)
        frozen=p.get('prepared',{})
        require(frozen.get('key')=='ArrowRight' and frozen.get('expires_in_ms')==10000,p)
        token=frozen.get('token')
        require(isinstance(token,str) and len(token)==42 and token.startswith('macos_key:')
            and all(c in '0123456789abcdef' for c in token[10:]),'invalid key token')
        return token
    def refused(token,name):
        before=state()
        result=call('press_macos_window_arrowright',binding=binding,token=token)
        report[name]=result
        require(result.get('ok') is False and result.get('action_attempted') is False
            and result.get('effects_unconfirmed') is False and result.get('posting_calls')==0,result)
        require(state()==before,'refused key changed fixture')
        checks[name]=True
    initial=evaluate('armArrowFixture()');report['initial']=initial
    require(initial['events']==[] and initial['caret']==2 and initial['count']==0,'fixture not pristine')
    old=prepare(); newer=prepare();refused(old,'refresh_refused');refused(newer,'consumed_on_wrong_token')
    token=prepare();evaluate("selectKeyboardTarget('second')");refused(token,'changed_receiver_refused')
    evaluate("selectKeyboardTarget('first')");token=prepare()
    evaluate("(()=>{const e=document.getElementById('first');const n=e.cloneNode(true);e.replaceWith(n);n.focus();return true;})()")
    refused(token,'replacement_refused')
    token=prepare();evaluate("document.getElementById('first').type='password'")
    refused(token,'protected_transition_refused')
    protected=call('prepare_macos_window_arrowright',binding=binding);report['protected_preparation']=protected
    require(protected.get('ok') is False,'protected field prepared a key')
    evaluate("document.getElementById('first').type='text'")
    before=evaluate('armArrowFixture()');report['before']=before
    token=prepare()
    native=call('press_macos_window_arrowright',binding=binding,token=token)
    report['native_dispatch']=native
    # Observation polling never resends the key, including after a partial result.
    until=time.monotonic()+2
    after=state()
    while len(after.get('events',[]))<2 and time.monotonic()<until:
        time.sleep(.05);after=state()
    report['after']=after;report['assessment']=verify_effect(native,before,after)
    refused(token,'replay_refused')
    # Keep one unused preparation for the caller's stale-binding check after unbind.
    pending=prepare();report['passed']=True
    return pending
