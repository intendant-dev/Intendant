#!/usr/bin/env python3
"""Cold-start pinned Rabby on three fresh proof displays. No keys or transactions."""
import argparse, fcntl, hashlib, json, os, pathlib, shutil, socket, subprocess, time

ARCHIVE_SHA = 'daf7819d7371a67ef447c788e899b1df628f95e380a460c6e5dd3b86bbe09e4f'
ARCHIVE_BYTES = 16216742
METRICS_JS = r'''
const [port,id]=process.argv.slice(1);
const controller=new AbortController(),timer=setTimeout(()=>controller.abort(),3000);
const targets=await(await fetch(`http://127.0.0.1:${port}/json/list`,{signal:controller.signal})).json();clearTimeout(timer);
const page=targets.find(p=>p.id===id && p.type==='page');if(!page)throw Error('bound page missing');
if(page.url.startsWith('chrome-extension:'))throw Error('onboarding selected instead of application');
const endpoint=new URL(page.webSocketDebuggerUrl);
if(endpoint.hostname!=='127.0.0.1'||endpoint.port!==port||endpoint.pathname!==`/devtools/page/${id}`)throw Error('foreign CDP endpoint');
const ws=new WebSocket(endpoint);const result=await new Promise((resolve,reject)=>{
const timeout=setTimeout(()=>reject(Error('CDP deadline')),3000);
ws.onerror=()=>reject(Error('CDP failed'));ws.onopen=()=>ws.send(JSON.stringify({id:1,method:'Page.getLayoutMetrics'}));
ws.onmessage=event=>{const value=JSON.parse(event.data);if(value.id===1){clearTimeout(timeout);if(value.error)reject(Error('metrics refused'));else resolve(value.result);}};
});ws.close();console.log(JSON.stringify(result));
'''

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('bin','archive','cache-root','capture-state-root','node','evidence'):
        parser.add_argument('--'+name,required=True,type=pathlib.Path)
    parser.add_argument('--target',default='https://example.org/')
    args=parser.parse_args()
    if not args.target.startswith('https://'):raise RuntimeError('HTTPS test target required')
    archive=args.archive.resolve(strict=True)
    if archive.stat().st_size!=ARCHIVE_BYTES or hashlib.sha256(archive.read_bytes()).hexdigest()!=ARCHIVE_SHA:
        raise RuntimeError('pinned Rabby archive does not match')
    if subprocess.run(['pgrep','-x','Runner.Worker'],stdout=subprocess.DEVNULL).returncode!=1:
        raise RuntimeError('CI has priority')
    os.umask(0o077)
    # Serialize with production capture. Do not remove a lock or cleanup quarantine.
    state=args.capture_state_root.resolve(strict=True)
    lock=(state/'capture.lock').open('r+')
    fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    if (state/'cleanup-pending').exists() and any((state/'cleanup-pending').iterdir()):
        raise RuntimeError('capture cleanup is pending')
    out=args.evidence.absolute();out.mkdir(mode=0o700)
    home=out/'home';home.mkdir(mode=0o700)
    binary=str(args.bin.resolve(strict=True));node=str(args.node.resolve(strict=True))
    env={'PATH':'/usr/local/bin:/usr/bin:/bin','HOME':str(home),'INTENDANT_HOME':str(home/'.intendant'),
         'LANG':'C.UTF-8','XDG_CACHE_HOME':str(args.cache_root.resolve(strict=True))}
    with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
    log=(out/'daemon.log').open('wb')
    daemon=subprocess.Popen([binary,'--web',str(port),'--bind','127.0.0.1','--no-tls','--no-presence'],env=env,cwd=out,stdin=subprocess.DEVNULL,stdout=log,stderr=subprocess.STDOUT)
    def ctl(*argv):
        p=subprocess.run([binary,'ctl','--port',str(port),'--json',*argv],env=env,cwd=out,stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=30)
        if p.returncode:raise RuntimeError('test control failed; private daemon log retained')
        value=json.loads(p.stdout)
        if isinstance(value,dict) and value.get('ok') is False:raise RuntimeError(str(value.get('error','tool refused')))
        return value
    display=browser=None;results=[];clean=True
    try:
        for _ in range(60):
            try:ctl('whoami');break
            except Exception:
                if daemon.poll() is not None:raise RuntimeError('test daemon exited')
                time.sleep(.2)
        baseline=ctl('display','list')
        for n in range(3):
            parent=out/f'attempt-{n}';parent.mkdir(mode=0o700)
            display=ctl('display','create','--width','1920','--height','1080','--min-display-id','120','--max-display-id','159')
            browser=ctl('browser','create',args.target,'--provider','cdp','--display-target',display['display_target'],
                '--session',f'viewport-rabby-{n}','--profile-dir',str(parent/'browser-profile'),'--viewport','1024x768',
                '--extension-archive',str(archive),'--extension-sha256',ARCHIVE_SHA,'--extension-bytes',str(ARCHIVE_BYTES),
                '--extension-manifest-version','3','--extension-version','0.94.6')
            p=subprocess.run([node,'--input-type=module','-e',METRICS_JS,str(browser['debugging_port']),browser['active_target_id']],stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=10)
            if p.returncode:raise RuntimeError('independent CDP metrics check failed')
            metrics=json.loads(p.stdout);css=metrics['cssLayoutViewport']
            if (css['clientWidth'],css['clientHeight'])!=(1024,768):raise RuntimeError('native viewport mismatch')
            (parent/'browser.json').write_text(json.dumps(browser));(parent/'metrics.json').write_text(json.dumps(metrics))
            results.append({'attempt':n,'width':css['clientWidth'],'height':css['clientHeight'],'extensionRuntimeId':browser['extension']['runtime_id']})
            if ctl('browser','close',browser['id'],'--reason','viewport-regression')['status']!='closed':raise RuntimeError('browser close failed')
            browser=None
            if not ctl('display','destroy',str(display['display_id']),display['capture_generation'],'--note','viewport-regression')['ok']:raise RuntimeError('exact teardown failed')
            display=None
            if (parent/'browser-profile').exists():shutil.rmtree(parent/'browser-profile')
        if ctl('display','list')!=baseline:
            clean=False
            raise RuntimeError('display leaked')
        result={'passed':True,'attempts':results,'privateKeysGenerated':False,'transactionsRequested':False,
                'binarySha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()}
        (out/'RESULT.json').write_text(json.dumps(result));print(json.dumps(result),flush=True)
    except BaseException:
        clean=False
        raise
    finally:
        if browser:
            try:clean = (ctl('browser','close',browser['id'],'--reason','viewport-failed-cleanup').get('status')=='closed') and clean
            except Exception:clean=False
        if display:
            try:clean = (ctl('display','destroy',str(display['display_id']),display['capture_generation'],'--note','viewport-failed-cleanup').get('ok') is True) and clean
            except Exception:clean=False
        daemon.terminate()
        try:daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:daemon.kill();daemon.wait(timeout=5)
        log.close()
        (out/'CLEANUP.json').write_text(json.dumps({'exactResourceCleanupConfirmed':clean,'testDaemonStopped':True}))
        lock.close()

if __name__=='__main__':main()
