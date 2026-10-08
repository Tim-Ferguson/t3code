#!/usr/bin/env python3
"""Deterministic native v3 sidecar fixture, using the production owner path."""
import json, os, sys, signal
MODE = "__MODE__"
LOG = "__LOG__"
if MODE=="hold-term":signal.signal(signal.SIGTERM,signal.SIG_IGN)
hello = dict(version=3, type="hello", sidecarVersion="fixture", sidecarPid=os.getpid(), platform=sys.platform, arch="fixture", capabilities={key:True for key in ("cumulativeCpuTime", "currentCpuPercent", "residentMemory", "virtualMemory", "ioBytes", "processStartTime", "processTree")})
def emit(value):
    sys.stdout.write(json.dumps(value)+"\n"); sys.stdout.flush()
def snapshot(request=None):
    value=dict(version=3,type="snapshot",sequence=1,sampledAtUnixMs=1000,collectionDurationMicros=1,scannedProcessCount=0,retainedProcessCount=0,inaccessibleProcessCount=0,processes=[])
    if request is not None:value["requestId"]=request
    return value
if MODE=="bad-version":hello["version"]=2
if MODE=="whitespace":sys.stdout.write(" \n");sys.stdout.flush()
elif MODE!="nohello":emit(hello)
for line in sys.stdin:
    command=json.loads(line)
    if LOG:
        with open(LOG,"a") as log:log.write(json.dumps(command)+"\n")
    kind=command["type"]
    if kind=="sampleNow":
        emit(snapshot(command["requestId"]))
        if MODE=="restart" and not os.path.exists(LOG+".restarted"):
            open(LOG+".restarted","w").close();sys.exit(7)
    elif kind=="processTable":
        if MODE not in ("hold","timeout"):emit(dict(version=3,type=kind,requestId=command["requestId"],processes=[dict(pid=os.getpid(),ppid=os.getppid(),name="fixture")]))
    elif kind=="readHistory":
        emit(dict(version=3,type="historyChunk",requestId=command["requestId"],done=False,snapshots=[snapshot()]))
        emit(dict(version=3,type="historyChunk",requestId=command["requestId"],done=True,snapshots=[snapshot()]))
    elif kind=="setStreaming" and command["enabled"]:emit(snapshot())
    elif kind=="shutdown":sys.exit(0)

if MODE=="hold-term":signal.pause()
