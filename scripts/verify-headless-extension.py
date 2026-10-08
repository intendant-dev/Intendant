#!/usr/bin/env python3
"""Fixed no-wallet extension acceptance via a real supervised session.

Tests only the deterministic repository extension and local page.
No account, credentials, financial operation or third-party package is used.
Verifies actual toolbar popup and extension-created notification UI.
"""
from __future__ import annotations
import os,sys,subprocess,tempfile,time,json,re,hashlib,uuid,shlex,threading,http.server,importlib.util,shutil,base64
from pathlib import Path
import argparse,zipfile
parser=argparse.ArgumentParser(description="Opt-in isolated supervised-session extension test; no keys, signatures, funds or native display/input.")
parser.add_argument('--bin',required=True,type=Path)
parser.add_argument('--browser-app',required=True,type=Path)
parser.add_argument('--report',required=True,type=Path)
parser.add_argument('--allow-offscreen-extension',action='store_true')
args=parser.parse_args()
if sys.platform!='darwin' or not args.allow_offscreen_extension or args.report.exists():parser.error('requires macOS, an explicit opt-in, and a fresh report path')
os.umask(0o077)
repo=Path(__file__).resolve().parent.parent;sys.path.insert(0,str(repo/'scripts'))
def load(name,path):
 sp=importlib.util.spec_from_file_location(name,repo/'scripts'/path);m=importlib.util.module_from_spec(sp);sys.modules[name]=m;sp.loader.exec_module(m);return m
h=load('extension_task_harness','verify-macos-task-browser.py');probe=load('extension_session_client','verify-macos-browser-delegation.py');cdplib=load('extension_observer','verify-macos-chromium-controls.py')
root=Path(tempfile.mkdtemp(prefix='intendant-extension-session-'));root.chmod(0o700)
binary=args.bin.resolve(strict=True)
archive=root/'fixture.zip';version='1.0.0';worker='worker.js'
with zipfile.ZipFile(archive,'w',zipfile.ZIP_DEFLATED) as z:
 for p in sorted((repo/'tests/fixtures/macos-monitor/extension').iterdir()):
  if p.is_file():z.writestr(zipfile.ZipInfo(p.name,date_time=(2020,1,1,0,0,0)),p.read_bytes())
digest=hashlib.sha256(archive.read_bytes()).hexdigest()
report={'ok':False,'scope':'synthetic_extension_complete','native_input_calls':0,'owner_input_requests':0,'seed_or_private_key_used':False,'transactions_requested':0,'signatures_requested':0,'source_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip(),'source_dirty':subprocess.run(['git','diff','--quiet','HEAD'],cwd=repo).returncode!=0,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'extension_sha256':digest,'extension_version':version,'steps':[],'rig':str(root)}
out=args.report.absolute();out.parent.mkdir(parents=True,exist_ok=True)
def save():out.write_text(json.dumps(report,indent=2)+'\n')
daemon=dashboard=cdp=web=None;sid=None;assigned=None;profile=None;port=None
try:
 try:report['foreground_before']=json.loads(subprocess.check_output([str(binary),'--private-macos-keyboard-foreground-v1'],text=True,timeout=5))
 except Exception:report['foreground_before']=None
 home=root/'home';home.mkdir();base=root/'base';base.mkdir();(base/'intendant.toml').write_text('');mock=root/'mock.json';mock.write_text('{"profiles":[]}')
 cache=home/'Library/Caches/intendant/browser-workspaces';cache.mkdir(parents=True);(cache/'Google Chrome for Testing.app').symlink_to(args.browser_app.resolve(strict=True),target_is_directory=True)
 policy=root/'policy.json';policy.write_text(json.dumps({'schema_version':1,'extensions':[{'archive_sha256':digest,'archive_byte_length':archive.stat().st_size,'manifest_version':3,'version':version,'service_worker':worker}]}));pin=hashlib.sha256(policy.read_bytes()).hexdigest()
 html=b'''<!doctype html><meta charset="utf-8"><title>Disposable extension test</title><h1>Extension fixture</h1><input id="plain" aria-label="Fixture text"><button id="notice" onclick="window.postMessage({type:'intendant-fixture-notification'},location.origin)">Open fixture notification</button>'''
 class Handler(http.server.BaseHTTPRequestHandler):
  def do_GET(self):
   self.send_response(200);self.send_header('Content-Type','text/html; charset=utf-8');self.send_header('Content-Length',str(len(html)));self.end_headers();self.wfile.write(html)
  def log_message(self,*unused):pass
 web=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=web.serve_forever,daemon=True).start();url=f'http://127.0.0.1:{web.server_port}/fixture'
 env={k:v for k,v in os.environ.items() if k in ['PATH','LANG','TMPDIR','USER','LOGNAME']};env.update(HOME=str(home),USERPROFILE=str(home),PROVIDER='mock',INTENDANT_MOCK_SCRIPT=str(mock),INTENDANT_MOCK_DISPLAY='synthetic',INTENDANT_MOCK_MEMORY='nominal')
 log=root/'daemon.log'
 with log.open('wb') as f:daemon=subprocess.Popen([str(binary),'--web','0','--bind','127.0.0.1','--no-tui','--no-tls','--autonomy','full','--browser-extension-policy',str(policy),'--browser-extension-policy-sha256',pin],cwd=base,env=env,stdin=subprocess.DEVNULL,stdout=f,stderr=f)
 for _ in range(400):
  match=re.search(r'Dashboard:.*?http://127\.0\.0\.1:(\d+)',log.read_text(errors='replace'))
  if match:
   port=int(match[1]);tokenpath=home/'.intendant/loopback-tokens'/f'{port}.token'
   if tokenpath.exists():token=tokenpath.read_text().strip();break
  assert daemon.poll() is None,'temporary daemon exited';time.sleep(.1)
 assert port,'no temporary daemon port';dashboard=h.Dashboard(port,token)
 project=root/'session';project.mkdir();(project/'intendant.toml').write_text('');backend=project/'backend';backend.write_text('#!/bin/sh\nexec '+shlex.quote(sys.executable)+' '+shlex.quote(str(repo/'scripts/verify-macos-browser-delegation.py'))+' "$@"\n');backend.chmod(0o700)
 (project/'.intendant-delegation-proof.json').write_text(json.dumps({'schema':probe.SCHEMA,'origin':f'http://127.0.0.1:{port}','expires_unix_ms':time.time_ns()//1000000+300000,'record_probes':True}));(project/'.intendant-delegation-proof.json').chmod(0o600)
 def issue(args,tool='task_browser',first=False,expect_error=False):
  args=dict(args)
  if args.get('op') not in ('open','status') and tool!='whoami':args.setdefault('workspace_id',assigned)
  if args.get('op') in ('extension_popup','extension_page','click','keyboard','scroll','navigate'):args.setdefault('request_id',str(uuid.uuid4()))
  rid=str(uuid.uuid4());task=json.dumps({'probe':{'id':rid,'tool':tool,'arguments':args}})
  dashboard.send({'action':'create_session','task':task,'agent':'claude-code','agent_command':str(backend),'project_root':str(project)} if first else {'action':'start_task','session_id':sid,'task':task})
  receipt=project/('.intendant-probe-'+rid+'.json');deadline=time.monotonic()+65
  value=None
  while time.monotonic()<deadline:
   if receipt.exists():
    try:value=json.loads(receipt.read_text());break
    except json.JSONDecodeError:pass
   assert daemon.poll() is None;time.sleep(.05)
  assert value is not None,'missing session receipt'
  report['steps'].append({'op':args.get('op',tool),'receipt':value});save()
  assert value.get('owner_fallback_used') is False and not value.get('probe_failed'),value
  assert bool(value.get('tool_error'))==expect_error,value
  return value
 who=issue({},'whoami',True);sid=who['token_bound_session']
 opened=issue({'op':'open','url':url,'extension':{'archive_path':str(archive),'archive_sha256':digest,'archive_byte_length':archive.stat().st_size,'manifest_version':3,'version':version}})['texts'][0];assigned=opened['workspace_id'];report['open']=opened
 assert opened.get('backend')=='headless_extension' and opened.get('ready') is True
 profile=home/'Library/Application Support/intendant/browser-workspaces'/assigned/'profile';lines=(profile/'DevToolsActivePort').read_text().splitlines();cdp=cdplib.CDP(int(lines[0]),lines[1]);original=[t for t in cdp.call('Target.getTargets')['targetInfos'] if t['type']=='page' and t['url']==url][0]['targetId']
 observer={}
 def eval_target(target,expression):
  if target not in observer:observer[target]=cdp.call('Target.attachToTarget',{'targetId':target,'flatten':True})['sessionId']
  result=cdp.call('Runtime.evaluate',{'expression':expression,'returnByValue':True},session=observer[target]);assert 'exceptionDetails' not in result,result
  return result['result'].get('value')
 issue({'op':'navigate','url':url+'?extension-ready=1'})
 def view():
  views=issue({'op':'extension_views'},'inspect_task_browser')['texts'][0]['views'];assert views,'no extension UI view'
  targetrows=[t for t in cdp.call('Target.getTargets')['targetInfos'] if t['type']=='page' and t['url'].startswith('chrome-extension:')]
  chosen=next((v for v in views if v['resource']=='/index.html'),views[0]);rows=[t for t in targetrows if __import__('urllib.parse',fromlist=['urlsplit']).urlsplit(t['url']).path==chosen['resource']];assert len(rows)==1,(views,targetrows)
  return chosen['view_id'],rows[0]['targetId']
 def click_text(text,handle,target):
  expr='''(()=>{const wanted=TEXT;const es=[...document.querySelectorAll('button,a,[role="button"],div,span')].filter(e=>e.textContent.trim()===wanted).map(e=>({e,r:e.getBoundingClientRect()})).filter(({r})=>r.width>1&&r.height>1);es.sort((a,b)=>a.r.width*a.r.height-b.r.width*b.r.height);if(!es.length)return null;const r=es[0].r;return {x:r.x+r.width/2,y:r.y+r.height/2}})()'''.replace('TEXT',json.dumps(text))
  pt=eval_target(target,expr);assert pt,'no control '+text;issue({'op':'click','view_id':handle,**pt})
 issue({'op':'extension_popup'})
 time.sleep(.5);handle,target=view()
 # Only the repository fixture exposes this independent readback function.
 state=eval_target(target,'window.extensionFixtureStatus()')
 assert state['activeUrl']==url+'?extension-ready=1',state
 report['actual_popup_has_original_tab_context']=True
 shot=issue({'op':'screenshot','view_id':handle},'inspect_task_browser');assert len(shot['images'])==1
 issue({'op':'keyboard','view_id':handle,'action':{'type':'select_all'}})
 issue({'op':'keyboard','view_id':handle,'action':{'type':'insert_text','text':'Offscreen extension typing 🙂'}})
 assert eval_target(target,'window.extensionFixtureStatus().text')=='Offscreen extension typing 🙂'
 issue({'op':'keyboard','view_id':handle,'action':{'type':'key','key':'Tab'}})
 issue({'op':'keyboard','view_id':handle,'action':{'type':'key','key':'Enter'}})
 for _ in range(50):
  state=eval_target(target,'window.extensionFixtureStatus()')
  if state['saved']==1:break
  time.sleep(.1)
 assert state['saved']==1 and state['text']=='Offscreen extension typing 🙂',state
 report['popup_keyboard_and_worker_effects']=True
 foreign=issue({'op':'screenshot','view_id':'bv-not-assigned'},'inspect_task_browser',expect_error=True)
 report['foreign_view_refused']=foreign.get('tool_error') is True
 state=eval_target(target,'window.extensionFixtureStatus()');rect=state['buttonRect']
 once={'op':'click','view_id':handle,'request_id':str(uuid.uuid4()),'x':rect['x']+rect['width']/2,'y':rect['y']+rect['height']/2}
 issue(once)
 for _ in range(30):
  if eval_target(target,'window.extensionFixtureStatus().saved')==2:break
  time.sleep(.05)
 assert eval_target(target,'window.extensionFixtureStatus().saved')==2
 issue(once,expect_error=True)
 assert eval_target(target,'window.extensionFixtureStatus().saved')==2
 report['duplicate_click_not_replayed']=True
 # The real popup is intentionally allowed to close on its own focus lifecycle.
 # The site issues the fixture request; the extension creates a focused popup
 # window inside the headless browser. No synthetic popup.html tab is used.
 pt=eval_target(original,'(()=>{const r=document.querySelector("#notice").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()')
 issue({'op':'click',**pt})
 notice=None
 for _ in range(60):
  views=issue({'op':'extension_views'},'inspect_task_browser')['texts'][0]['views']
  notice=next((v for v in views if v['resource']=='/notification.html'),None)
  if notice:break
  time.sleep(.1)
 assert notice,'extension notification window not discovered'
 note_target=next(t['targetId'] for t in cdp.call('Target.getTargets')['targetInfos'] if t['type']=='page' and t['url'].endswith('/notification.html'))
 shot=issue({'op':'screenshot','view_id':notice['view_id']},'inspect_task_browser');assert len(shot['images'])==1
 click_text('Acknowledge fixture',notice['view_id'],note_target)
 for _ in range(40):
  accepted=eval_target(original,'document.documentElement.dataset.notificationAccepted')
  if accepted=='true':break
  time.sleep(.1)
 assert accepted=='true','notification acknowledgement not independently observed'
 report['extension_created_notification_clicked']=True
 other=issue({},'whoami',first=True);owner_sid=sid;sid=other['token_bound_session']
 assert sid!=owner_sid
 rejected=issue({'op':'keyboard','action':{'type':'insert_text','text':'must not appear'}},expect_error=True)
 report['other_session_refused']=rejected.get('tool_error') is True
 dashboard.send({'action':'stop_session','session_id':sid});sid=owner_sid
 report['ok']=True
except Exception as e:report['error']=str(e)
finally:
 if cdp:cdp.close()
 if dashboard and sid:dashboard.send({'action':'stop_session','session_id':sid})
 if profile:
  for _ in range(120):
   if not profile.exists():break
   time.sleep(.1)
  report['task_stop_profile_removed']=not profile.exists()
 if dashboard:dashboard.close()
 if daemon:
  daemon.terminate()
  try:daemon.wait(timeout=15)
  except subprocess.TimeoutExpired:daemon.kill();daemon.wait(timeout=5)
  report['daemon_reaped']=daemon.poll() is not None
 if web:web.shutdown();web.server_close()
 try:report['foreground_after']=json.loads(subprocess.check_output([str(binary),'--private-macos-keyboard-foreground-v1'],text=True,timeout=5))
 except Exception:report['foreground_after']=None
 report['foreground_endpoints_unchanged']=report.get('foreground_before') is not None and report.get('foreground_before')==report.get('foreground_after')
 report['human_typing_overlap_verified']=False
 report['ok']=report['ok'] and report.get('task_stop_profile_removed') is True and report.get('daemon_reaped') is True
 report['daemon_diagnostics']=(root/'daemon.log').read_text(errors='replace')[-2500:] if (root/'daemon.log').exists() else ''
 save();print('REPORT',out);print(json.dumps({k:v for k,v in report.items() if k not in ('steps','daemon_diagnostics')},indent=2))

raise SystemExit(0 if report['ok'] else 1)
