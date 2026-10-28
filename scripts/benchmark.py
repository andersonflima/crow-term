#!/usr/bin/env python3
"""Indicative headless PTY/VT measurements. NOT GUI/input latency or an A/B gain claim."""
import argparse
import json
import os
from pathlib import Path
import platform
import shlex
import statistics
import subprocess
import sys
import tempfile
import time

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary",type=Path,default=Path("target/release/crow-term"))
parser.add_argument("--iterations",type=int,default=5)
parser.add_argument("--lines",type=int,default=10000)
args=parser.parse_args()
if os.name=="nt":
    parser.error("POSIX fixture; Windows GUI/ConPTY measurements require a separate workload")
if not 5<=args.iterations<=100 or not 100<=args.lines<=1000000:
    parser.error("iterations5..100 and lines100..1000000 required")
binary=args.binary.resolve(strict=True)
samples=[]
for iteration in range(args.iterations+1):
    with tempfile.TemporaryDirectory(prefix="crow-bench-") as temp:
        path=Path(temp);path.chmod(0o700)
        # Empty explicit config prevents the measurement from importing user settings.
        config=path/"config.toml";config.write_text("")
        env=dict(os.environ,CROW_TERM_RUNTIME_DIR=temp)
        def cli(*params):
            result=subprocess.run([str(binary),"--config",str(config),"--shell","/bin/sh",*map(str,params)],env=env,capture_output=True,text=True,timeout=15)
            if result.returncode:raise RuntimeError(result.stderr)
            return result.stdout
        with (path/"daemon.log").open("w") as log:
            start=time.perf_counter()
            daemon=subprocess.Popen([str(binary),"--config",str(config),"daemon"],env=env,stdout=log,stderr=log)
            try:
                while True:
                    if daemon.poll() is not None:raise RuntimeError("daemon exited")
                    try:
                        cli("doctor") # doctor does not autostart another daemon
                        # The socket exists only after daemon has bound it.
                        if any(path.glob("daemon-v*.sock")):break
                    except RuntimeError:pass
                    if time.perf_counter()-start>10:raise TimeoutError("startup")
                    time.sleep(.002)
                ready=time.perf_counter()-start
                pane=json.loads(cli("new-session","bench"))["Created"]["pane"]
                payload="X"*70+"\n"
                program="import sys;sys.stdout.write("+repr(payload)+"*"+str(args.lines)+"+'\\nCROW_BENCH_END\\n');sys.stdout.flush()"
                command=shlex.quote(sys.executable)+" -c "+shlex.quote(program)
                start=time.perf_counter()
                cli("send-keys",pane,command,"--enter")
                while "\nCROW_BENCH_END\n" not in cli("capture-pane",pane):
                    if time.perf_counter()-start>30:raise TimeoutError("output workload")
                elapsed=time.perf_counter()-start
                rss=None
                if platform.system() in ("Darwin","Linux"):
                    rss=int(subprocess.check_output(["ps","-p",str(daemon.pid),"-o","rss="],text=True).strip())
                if iteration:
                    samples.append({"daemon_ready_ms":ready*1000,"pty_vt_workload_ms":elapsed*1000,"daemon_rss_kib":rss})
            finally:
                try:cli("stop","--force")
                finally:
                    try:daemon.wait(timeout=10)
                    except subprocess.TimeoutExpired:daemon.kill();daemon.wait()
def summary(name):
    values=[s[name] for s in samples if s[name] is not None]
    return {"min":min(values),"median":statistics.median(values),"max":max(values)} if values else None
print(json.dumps({"scope":"indicative headless PTY/parser+CLI polling; NOT rendering/input latency; no comparative gain claim",
                  "platform":platform.platform(),"architecture":platform.machine(),"binary_bytes":binary.stat().st_size,
                  "warmup_runs":1,"measured_runs":len(samples),"lines":args.lines,"bytes_per_line":71,
                  "controls":"fresh private daemon/runtime+empty config, sequential, same binary; shared host background load uncontrolled",
                  "summary":{key:summary(key) for key in samples[0]},"samples":samples},indent=2))
