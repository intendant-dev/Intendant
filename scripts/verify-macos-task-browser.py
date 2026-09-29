#!/usr/bin/env python3
"""Opt-in normal-session browser acceptance. Owner credentials only start/stop
sessions and independently observe/clean up the disposable rig. The child alone
opens its browser and sends all tested screenshot/pointer/keyboard operations.
No provider API calls, user profiles, global input or installed daemon changes.
"""
from __future__ import annotations
import argparse, base64, hashlib, http.client, http.server, importlib.util, json
import os, re, secrets, shlex, shutil, socket, struct, subprocess, sys, tempfile
import threading, time, urllib.parse, uuid
from pathlib import Path

def require(ok, message):
    if not ok: raise RuntimeError(message)

def load(name, filename):
    spec=importlib.util.spec_from_file_location(name,Path(__file__).with_name(filename))
    mod=importlib.util.module_from_spec(spec);sys.modules[name]=mod;spec.loader.exec_module(mod);return mod

class Dashboard:
    def __init__(self, port, token):
        self.sock=socket.create_connection(('127.0.0.1',port),timeout=5)
        key=base64.b64encode(secrets.token_bytes(16)).decode()
        path='/ws?'+urllib.parse.urlencode({'token':token})
        self.sock.sendall((f'GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
        data=b''
        while b'\r\n\r\n' not in data:
            require(len(data)<=8192,'dashboard handshake budget');chunk=self.sock.recv(4096)
            require(chunk,'dashboard handshake closed');data+=chunk
        header=data.split(b'\r\n\r\n',1)[0]
        require(header.split(b'\r\n')[0].split()[1]==b'101','dashboard handshake refused')
        expected=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        headers=dict((k.strip().lower(),v.strip()) for k,v in (line.split(b':',1) for line in header.split(b'\r\n')[1:]))
        require(headers.get(b'sec-websocket-accept')==expected,'dashboard handshake mismatch')
        # Drain only this private dashboard so its event backpressure cannot
        # affect the supervised fixture; no secrets are recorded.
        self.stopped=False
        def drain():
            while not self.stopped:
                try:
                    if not self.sock.recv(65536): return
                except socket.timeout: continue
                except OSError: return
        self.thread=threading.Thread(target=drain,daemon=True);self.thread.start()
    def send(self, message):
        data=json.dumps(message).encode();require(len(data)<65536,'dashboard request budget')
        mask=secrets.token_bytes(4);head=bytes([0x80|len(data)]) if len(data)<126 else b'\xfe'+struct.pack('!H',len(data))
        self.sock.sendall(b'\x81'+head+mask+bytes(v^mask[i%4] for i,v in enumerate(data)))
    def close(self):
        self.stopped=True;self.sock.close()


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--bin',required=True,type=Path);ap.add_argument('--browser-app',required=True,type=Path)
    ap.add_argument('--report',required=True,type=Path);ap.add_argument('--allow-shared-session-monitor',action='store_true')
    args=ap.parse_args()
    if sys.platform!='darwin' or not args.allow_shared_session_monitor or not __debug__:
        ap.error('requires macOS and explicit shared-session monitor opt-in')
    require(not args.report.exists(),'report must be fresh')
    binary=args.bin.resolve(strict=True);app=args.browser_app.resolve(strict=True)
    helper=load('task_keyboard_helper','verify-macos-managed-keyboard.py')
    mon=load('task_monitor_helper','verify-macos-monitor-http.py')
    browser=load('task_browser_helper','verify-macos-chromium-controls.py')
    probe=load('task_session_probe','verify-macos-browser-delegation.py')
    lock=helper.acquire_native_test_lock(Path(tempfile.gettempdir())/'intendant-managed-keyboard-acceptance.lock')
    root=Path(tempfile.mkdtemp(prefix='intendant-task-browser-proof-'));root.chmod(0o700)
    report={'ok':False,'before':mon.inventory(),'checks':{},'steps':[],
      'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
      'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
      'source_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=Path(__file__).resolve().parent.parent,text=True).strip(),
      'installed_daemon_changed':False,'owner_input_requests':0,'manual_assignment_requests':0,
      'automatic_input_retry':False,'human_typing_overlap_verified':False,'rig':str(root)}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    def save(): args.report.write_text(json.dumps(report,indent=2)+'\n')
    daemon=dashboard=cdp=web=None;port=token=None;assigned={}
    def owner(tool, arguments):
        c=http.client.HTTPConnection('127.0.0.1',port,timeout=65)
        try:
            c.request('POST','/mcp',json.dumps({'jsonrpc':'2.0','id':str(uuid.uuid4()),'method':'tools/call',
              'params':{'name':tool,'arguments':arguments}}),{'Content-Type':'application/json','x-intendant-loopback-token':token})
            r=c.getresponse();b=r.read(12*1024*1024+1);require(r.status==200 and len(b)<=12*1024*1024,'owner observation failed')
            data=json.loads(b);require('error' not in data,'owner observation protocol error')
            texts=[x['text'] for x in data['result'].get('content',[]) if x.get('type')=='text']
            return json.loads('\n'.join(texts))
        finally: c.close()
    def issue(project, sid, arguments, tool='task_browser', first=False):
        arguments=dict(arguments)
        if tool in ('task_browser','inspect_task_browser') and arguments.get('op') not in ('open','status'):
            arguments.setdefault('workspace_id',assigned.get(sid,'bw-no-assignment'))
        rid=str(uuid.uuid4());cmd={'id':rid,'tool':tool,'arguments':arguments}
        message={'action':'start_task','session_id':sid,'task':json.dumps({'probe':cmd})}
        if first: message={'action':'create_session','task':message['task'],'agent':'claude-code',
                          'agent_command':str(project/'backend'),'project_root':str(project)}
        dashboard.send(message)
        path=project/('.intendant-probe-'+rid+'.json');end=time.monotonic()+65
        while not path.exists() and time.monotonic()<end:
            require(daemon.poll() is None,'temporary daemon exited');time.sleep(.05)
        require(path.exists(),'supervised request produced no receipt')
        # Writer uses O_EXCL; wait only for its one bounded write to finish.
        value=None
        for _ in range(20):
            try: value=probe.loads(path.read_text());break
            except json.JSONDecodeError: time.sleep(.01)
        require(value is not None and value.get('owner_fallback_used') is False,'invalid session receipt')
        require(value.get('probe_failed') is not True,'session request failed before receipt')
        report['steps'].append({'op':arguments.get('op',tool),'receipt':value});save()
        return value
    def payload(value):
        require(value.get('tool_error') is not True,'task tool refused: '+str(value.get('texts')))
        texts=value.get('texts',[]);require(len(texts)==1 and isinstance(texts[0],dict),'missing structured task result')
        require(texts[0].get('ok') is not False,'task operation failed: '+str(texts[0]))
        return texts[0]
    def setup_project(path):
        path.mkdir();(path/'intendant.toml').write_text('')
        backend=path/'backend';backend.write_text('#!/bin/sh\nexec '+shlex.quote(sys.executable)+' '+shlex.quote(str(Path(__file__).with_name('verify-macos-browser-delegation.py').resolve()))+' "$@"\n');backend.chmod(0o700)
        manifest={'schema':probe.SCHEMA,'origin':f'http://127.0.0.1:{port}',
          'expires_unix_ms':time.time_ns()//1_000_000+300_000,'record_probes':True}
        fd=os.open(path/'.intendant-delegation-proof.json',os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
        with os.fdopen(fd,'w') as f:json.dump(manifest,f)
    try:
        home=root/'home';home.mkdir();project=root/'agent';project.mkdir();(project/'intendant.toml').write_text('')
        mock=root/'mock.json';mock.write_text('{"profiles":[]}')
        cache=home/'Library/Caches/intendant/browser-workspaces';cache.mkdir(parents=True);(cache/app.name).symlink_to(app,target_is_directory=True)
        contents=(Path(__file__).resolve().parent.parent/'tests/fixtures/macos-monitor/task-browser.html').read_bytes()
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path!='/fixture':self.send_error(404);return
                self.send_response(200);self.send_header('Content-Type','text/html; charset=utf-8');self.send_header('Content-Length',str(len(contents)));self.end_headers();self.wfile.write(contents)
            def log_message(self,*unused):pass
        web=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=web.serve_forever,daemon=True).start()
        env={k:v for k,v in os.environ.items() if k in ('PATH','TMPDIR','LANG','LC_ALL','USER','LOGNAME')}
        env.update(HOME=str(home),USERPROFILE=str(home),PROVIDER='mock',INTENDANT_MOCK_SCRIPT=str(mock),INTENDANT_MOCK_DISPLAY='synthetic',INTENDANT_MOCK_MEMORY='nominal')
        logpath=root/'daemon.log'
        with logpath.open('wb') as log:daemon=subprocess.Popen([str(binary),'--web','0','--bind','127.0.0.1','--no-tui','--no-tls','--autonomy','full'],cwd=project,env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log)
        end=time.monotonic()+40
        while time.monotonic()<end:
            m=re.search(r'Dashboard:.*?http://127\.0\.0\.1:(\d+)',logpath.read_text(errors='replace'))
            if m:
                port=int(m[1]);p=home/'.intendant/loopback-tokens'/f'{port}.token'
                if p.exists():token=p.read_text().strip();break
            require(daemon.poll() is None,'temporary daemon exited before readiness');time.sleep(.1)
        require(port and token,'temporary daemon did not become ready')
        dashboard=Dashboard(port,token)
        # setup_project creates a new directory; retain daemon cwd separately.
        project=root/'session-one';setup_project(project)
        who=issue(project,None,{},tool='whoami',first=True);sid=who['token_bound_session']
        require(who['texts'][0]['actor_kind']=='agent_session' and who['texts'][0]['supervised'] is True,'not a supervised agent')
        report['checks']['authentic_session']=True
        url=f'http://127.0.0.1:{web.server_port}/fixture'
        opened=payload(issue(project,sid,{'op':'open','url':url}));require(opened['ready'],'task did not get a ready browser')
        assigned[sid]=opened['workspace_id']
        report['checks']['automatic_provision']=True
        workspaces=owner('list_browser_workspaces',{});work=[w for w in workspaces if w['id']==opened['workspace_id']]
        require(len(work)==1,'task workspace not unique');workspace=work[0];profile=Path(workspace['profile_dir'])
        report['private_profile']=str(profile)
        lines=(profile/'DevToolsActivePort').read_text().splitlines();cdp=browser.CDP(int(lines[0]),lines[1])
        cdp_session=cdp.call('Target.attachToTarget',{'targetId':workspace['active_target_id'],'flatten':True})['sessionId']
        def state():return cdp.call('Runtime.evaluate',{'expression':'window.taskFixtureStatus()','returnByValue':True},session=cdp_session)['result']['value']
        initial=state();require(initial['first']=='seed' and initial['count']==0,'fixture not pristine')
        shot=issue(project,sid,{'op':'screenshot'},tool='inspect_task_browser');payload(shot);require(len(shot['images'])==1,'session did not receive screenshot')
        report['checks']['session_screenshot']=True
        def keyboard(action):return payload(issue(project,sid,{'op':'keyboard','request_id':str(uuid.uuid4()),'action':action}))
        keyboard({'type':'select_all'});keyboard({'type':'insert_text','text':'Independent agent typing 🙂'})
        require(state()['first']=='Independent agent typing 🙂','typed text not observed')
        keyboard({'type':'key','key':'Tab'});keyboard({'type':'insert_text','text':'Second field!'})
        keyboard({'type':'key','key':'Backspace'});require(state()['second']=='Second field','Backspace effect missing')
        keyboard({'type':'key','key':'Tab'});keyboard({'type':'key','key':'Enter'})
        require(state()['count']==1,'Enter did not activate the focused button exactly once')
        report['checks']['session_keyboard']=True
        status=state();m=status['metrics'];require(m['scale']==1 and m['outerWidth']==720,'unexpected coordinate scale')
        def point(rect):return {'x':rect['x']+rect['width']/2,'y':m['outerHeight']-m['innerHeight']+rect['y']+rect['height']/2}
        clickid=str(uuid.uuid4());click={'op':'click','request_id':clickid,**point(status['canvasRect'])}
        payload(issue(project,sid,click));require(state()['canvasClicks']==1,'native coordinate click effect missing')
        repeated=issue(project,sid,click);require(repeated.get('tool_error') is True and state()['canvasClicks']==1,'duplicate click was replayed')
        payload(issue(project,sid,{'op':'scroll','request_id':str(uuid.uuid4()),'delta_y':180,**point(status['paneRect'])}))
        time.sleep(.2);require(state()['scroll']>0,'scroll effect missing')
        report['checks']['session_pointer_and_no_replay']=True
        finalshot=issue(project,sid,{'op':'screenshot'},tool='inspect_task_browser');payload(finalshot);require(len(finalshot['images'])==1,'final screenshot missing')
        other=root/'session-two';setup_project(other);other_who=issue(other,None,{},tool='whoami',first=True);other_sid=other_who['token_bound_session']
        missing=payload(issue(other,other_sid,{'op':'status'}));require(missing.get('workspace') is None,'other session saw task workspace')
        refused=issue(other,other_sid,{'op':'keyboard','workspace_id':opened['workspace_id'],'request_id':str(uuid.uuid4()),'action':{'type':'insert_text','text':'must not appear'}})
        require(refused.get('tool_error') is True and state()['second']=='Second field','foreign session gained access')
        report['checks']['foreign_session_refused']=True
        report['final_page']=state();cdp.close();cdp=None
        # Stop the owning session WITHOUT a separate close/grant call. Lifecycle
        # cleanup must terminate the browser and remove its private profile/monitor.
        dashboard.send({'action':'stop_session','session_id':sid});dashboard.send({'action':'stop_session','session_id':other_sid})
        end=time.monotonic()+40
        while time.monotonic()<end:
            inv=owner('list_macos_monitors',{})
            if inv.get('monitors')==[] and not profile.exists():break
            time.sleep(.25)
        require(inv.get('monitors')==[] and not profile.exists(),'task-stop cleanup incomplete')
        report['checks']['task_stop_cleanup']=True
        report['ok']=True
    except Exception as exc:
        report['error_type']=type(exc).__name__;report['error']=str(exc)
    finally:
        if cdp:cdp.close()
        if dashboard:dashboard.close()
        if daemon:
            if daemon.poll() is None:
                daemon.terminate()
                try:daemon.wait(timeout=15)
                except subprocess.TimeoutExpired:daemon.kill();daemon.wait(timeout=5)
            report['temporary_daemon_reaped']=daemon.poll() is not None
        if web:web.shutdown();web.server_close()
        end=time.monotonic()+8
        report['after']=mon.inventory()
        while report['after']!=report['before'] and time.monotonic()<end:
            time.sleep(.2);report['after']=mon.inventory()
        report['inventory_restored']=report['before']==report['after']
        report['ok']=report['ok'] and report['inventory_restored'] and report.get('temporary_daemon_reaped',False)
        save();lock.close()
        if report['ok']:shutil.rmtree(root)
    print(json.dumps(report,indent=2));return 0 if report['ok'] else 1
if __name__=='__main__':raise SystemExit(main())
