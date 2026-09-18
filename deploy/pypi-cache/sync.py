"""Serialized index-only Shadowmire followed by the Yukina hot-package cache."""
import fcntl
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from logs import prepare

UPSTREAM='https://mirrors.tuna.tsinghua.edu.cn/pypi/web/'
BUDGET=512*1024**3

def run(command):
    print('Running:', ' '.join(command), flush=True)
    process=subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    missing=set()
    last_progress=time.monotonic()
    suppressed=0
    def terminate(signum, _frame):
        process.send_signal(signum)
        raise SystemExit(128+signum)
    previous={s:signal.signal(s,terminate) for s in (signal.SIGTERM,signal.SIGINT)}
    try:
        for line in process.stdout:
            important=any(s in line for s in ('ERROR', 'WARN', 'SYNORA_', 'failed', 'All done', 'Success:'))
            progress=('updating ' in line or '%|' in line or 'Downloading' in line or 'Downloaded:' in line or '\x1b[' in line or line.lstrip().startswith('['))
            if progress and not important:
                suppressed+=1
                if time.monotonic()-last_progress >= 30:
                    print(f'Progress: {suppressed} activity lines since last report; {line.strip()[-160:]}',flush=True)
                    last_progress=time.monotonic();suppressed=0
            else:
                print(line, end='', flush=True)
            if line.startswith('SYNORA_MISSING='):
                missing.add(line.strip().split('=',1)[1])
        code=process.wait()
        if code: raise RuntimeError(f'{command[0]} exited with status {code}')
        return sorted(missing)
    finally:
        if process.poll() is None:
            process.terminate()
            try: process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait()
        for s,handler in previous.items(): signal.signal(s,handler)
        process.stdout.close()

def cache_command(root,logdir,upstream,budget):
    return ['yukina','--name','pypi','--repo-path',str(root/'packages'),'--size-limit',str(budget),'--url',upstream+'packages/','--strip-prefix','/packages','--filter',r'^[0-9a-f]{2}/[0-9a-f]{2}/[0-9a-f]+/[^/]+$','--log-path',str(logdir),'--log-format','mirror-json','--log-duration','7d','--min-vote-count','1','--include-browser-ua','--remote-sizedb',str(root/'.synora/remote-size.db'),'--local-sizedb',str(root/'.synora/local-size.db'),'--download-error-threshold','1','--output-stats']

def cycle(root,source,historical,upstream,budget,index_mode='full'):
    state=root/'.synora'; state.mkdir(parents=True,exist_ok=True)
    logdir=state/'logs';logdir.mkdir(exist_ok=True)
    with (state/'sync.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        count=prepare(source,historical,logdir/'pypi.log')
        print(f'Validated {count} recent requests',flush=True)
        if index_mode == 'full':
            run([sys.executable,__file__,'--shadowmire','--repo',str(root),'sync','--no-sync-packages','--no-filter-metadata','--shadowmire-upstream',upstream])
        elif index_mode != 'proxy':
            raise ValueError('PYPI_INDEX_MODE must be full or proxy')
        else:
            print('Index mode: on-demand upstream proxy; updating only access-log package candidates',flush=True)
        # Initial index sync can take days; refresh the seven-day vote window afterwards.
        if index_mode == 'full':
            prepare(source,historical,logdir/'pypi.log')
        (root/'packages').mkdir(exist_ok=True)
        missing=run(cache_command(root,logdir,upstream,budget)) or []
        if not isinstance(missing, list): missing=[]
        warning_tmp=state/'cache-warnings.tmp'
        warning_tmp.write_text(json.dumps(dict(count=len(missing),paths=missing))+'\n')
        warning_tmp.replace(state/'cache-warnings.json')
        if missing:
            print('SYNORA_STATUS=success_with_warnings',flush=True)
            print(f'SYNORA_MESSAGE={len(missing)} upstream package files missing; paths in .synora/cache-warnings.json',flush=True)
        marker=state/'initial-success.json'
        if index_mode == 'full' and not marker.exists() and not any(p.is_file() and not p.name.endswith('.tmp') for p in (root/'packages').glob('*/*/*/*')):
            raise RuntimeError('no hot package cached yet; initial sync is not complete')
        result=dict(completed_at=int(time.time()),cache_budget_bytes=budget,upstream=upstream,index_mode=index_mode)
        temp=state/'last-success.tmp';temp.write_text(json.dumps(result)+'\n');temp.replace(state/'last-success.json')
        if index_mode == 'full' and not marker.exists():
            temp.write_text(json.dumps(result)+'\n');temp.replace(marker)
        print('PyPI synchronization complete (index mode: '+index_mode+')',flush=True)

def shadowmire():
    # Upstream treats None from missing/failed metadata as successful parallel work.
    # Metadata is essential to this mirror; preserve resumability but fail the run.
    from shadowmire.sync.plain_http import SyncPlainHTTP
    original=SyncPlainHTTP.do_update
    def strict_update(self,*args,**kwargs):
        serial=original(self,*args,**kwargs)
        if serial is None: raise RuntimeError(f'critical metadata update failed: {args[0]}')
        return serial
    SyncPlainHTTP.do_update=strict_update
    original_simple=SyncPlainHTTP.get_package_simple
    def strict_simple(self,*args,**kwargs):
        try: return original_simple(self,*args,**kwargs)
        except Exception as exc:
            raise RuntimeError(f'critical simple metadata failed: {args[0]}') from exc
    SyncPlainHTTP.get_package_simple=strict_simple
    from shadowmire.cli import main
    sys.argv=[sys.argv[0]]+sys.argv[2:]
    main()

if __name__=='__main__':
    try:
        if len(sys.argv)>1 and sys.argv[1]=='--shadowmire': shadowmire()
        else:
            budget=int(os.environ.get('PYPI_CACHE_BYTES',BUDGET))
            if budget<=0: raise ValueError('PYPI_CACHE_BYTES must be positive')
            cycle(Path('/data'),Path('/nginx-log'),Path('/nginx-legacy'),os.environ.get('PYPI_UPSTREAM',UPSTREAM).rstrip('/')+'/',budget,os.environ.get('PYPI_INDEX_MODE','full'))
    except Exception as exc:
        print(f'PyPI synchronization failed: {exc}',file=sys.stderr,flush=True)
        sys.exit(1)
