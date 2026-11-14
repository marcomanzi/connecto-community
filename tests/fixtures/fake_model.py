#!/usr/bin/env python3
# Derived from Architect scratch probe; local fake model only, never real credentials.
# Usage: python3 fake_model.py PORT METADATA_LOG TOOL_COMMAND_FILE
# A prompt containing RUNEDIT uses the CLI's own file tools instead of its shell:
# Claude reads and edits seed.txt, then writes added.txt; Codex patches the same
# two. For it the command file holds the project's absolute path.
# Requests and tool results are never persisted; only lengths/digests are logged.
import json,sys,time,threading,hashlib
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class MetadataLog:
    def __init__(self, path):
        self.stream = open(path, 'a', buffering=1)
        self.lock = threading.Lock()
    def write(self, value):
        # Keep only fixed event family, byte count, and digest, never body/header data.
        data = value.encode()
        family = value.split(' ', 1)[0]
        if family not in {'GET', 'POST', 'ANTH', 'OAI'}:
            family = 'other'
        record = {'family': family, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
        with self.lock:
            self.stream.write(json.dumps(record) + '\n')
LOG=MetadataLog(sys.argv[2])
TOOLCMD=open(sys.argv[3]).read().strip()
# A prompt containing FLOOD streams this many lines, for render load tests.
FLOOD_LINES=8000
FLOOD_PACE=0.004
FLOOD_TEXT="flood line %06d the quick brown fox jumps over the lazy dog 0123456789\n\n"
PATCH="*** Begin Patch\n*** Update File: seed.txt\n@@\n-one\n+two\n*** Add File: added.txt\n+added\n*** End Patch\n"
def edit_steps():
    return [("Read",{"file_path":TOOLCMD+"/seed.txt"}),
            ("Edit",{"file_path":TOOLCMD+"/seed.txt","old_string":"one","new_string":"two"}),
            ("Write",{"file_path":TOOLCMD+"/added.txt","content":"added\n"})]
def flat(c):
    if isinstance(c,str): return c
    if isinstance(c,list): return ' '.join(flat(x.get('text') or x.get('content') or '') if isinstance(x,dict) else str(x) for x in c)
    return str(c)
class H(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*a): pass
    def sse(self,evs):
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.send_header('connection','close'); self.end_headers()
        for name,d in evs:
            self.wfile.write(f"event: {name}\ndata: {json.dumps(d)}\n\n".encode()); self.wfile.flush()
        self.close_connection=True
    def stream(self,head,delta,tail):
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.send_header('connection','close'); self.end_headers()
        def emit(name,d):
            self.wfile.write(f"event: {name}\ndata: {json.dumps(d)}\n\n".encode()); self.wfile.flush()
        for name,d in head: emit(name,d)
        for i in range(FLOOD_LINES):
            emit(*delta(FLOOD_TEXT%i)); time.sleep(FLOOD_PACE)
        for name,d in tail: emit(name,d)
        self.close_connection=True
    def do_GET(self):
        LOG.write(f"GET {self.path}\n"); b=b'{"data":[],"models":[]}'
        self.send_response(200); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); body=self.rfile.read(n)
        try: j=json.loads(body)
        except Exception: j={}
        LOG.write(f"POST {self.path} model={j.get('model')} stream={j.get('stream')}\n")
        if 'count_tokens' in self.path:
            b=b'{"input_tokens":10}'; self.send_response(200); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b); return
        if self.path.startswith('/v1/messages'): return self.anth(j)
        if 'responses' in self.path: return self.oai(j)
        b=b'{}'; self.send_response(200); self.send_header('content-length','2'); self.end_headers(); self.wfile.write(b)
    def anth(self,j):
        msgs=j.get('messages',[]); last=msgs[-1] if msgs else {}
        txt=flat(last.get('content','')); hastr='tool_result' in json.dumps(msgs)
        LOG.write("ANTH last="+txt[:300].replace('\n','\\n')+"\n")
        LOG.write("ANTH ntools=%d names=%s RUN=%s hastr=%s\n"%(len(j.get('tools') or []),[t.get('name') for t in (j.get('tools') or [])][:12],'RUNTOOL' in json.dumps(last),hastr))
        if hastr: LOG.write("ANTH toolresult="+json.dumps([m for m in msgs if 'tool_result' in json.dumps(m)])[:2200]+"\n")
        usage={"input_tokens":5,"output_tokens":5}
        start=("message_start",{"type":"message_start","message":{"id":"msg_f","type":"message","role":"assistant","model":j.get('model'),"content":[],"stop_reason":None,"stop_sequence":None,"usage":usage}})
        if 'FLOOD' in json.dumps(msgs):
            return self.stream([start,("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})],
             lambda t:("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":t}}),
             [("content_block_stop",{"type":"content_block_stop","index":0}),("message_delta",{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":None},"usage":usage}),("message_stop",{"type":"message_stop"})])
        done=json.dumps(msgs).count('"tool_result"')
        if 'RUNEDIT' in json.dumps(msgs) and done<len(edit_steps()):
            name,args=edit_steps()[done]
            return self.sse([start,("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_e%d"%done,"name":name,"input":{}}}),
             ("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json.dumps(args)}}),
             ("content_block_stop",{"type":"content_block_stop","index":0}),
             ("message_delta",{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":None},"usage":usage}),("message_stop",{"type":"message_stop"})])
        if 'RUNTOOL' in json.dumps(msgs) and 'tool_result' not in json.dumps(msgs):
            ev=[start,("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}),
             ("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json.dumps({"command":TOOLCMD,"description":"probe"})}}),
             ("content_block_stop",{"type":"content_block_stop","index":0}),
             ("message_delta",{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":None},"usage":usage}),("message_stop",{"type":"message_stop"})]
        else:
            r="FAKE-CLAUDE-REPLY to: "+txt[-60:].replace('\n',' ')
            ev=[start,("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
             ("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":r}}),
             ("content_block_stop",{"type":"content_block_stop","index":0}),
             ("message_delta",{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":None},"usage":usage}),("message_stop",{"type":"message_stop"})]
        self.sse(ev)
    def oai(self,j):
        inp=j.get('input',[]); s=json.dumps(inp); last=inp[-1] if inp else {}
        hastr=isinstance(last,dict) and 'output' in last
        LOG.write("OAI last="+json.dumps(last)[:900]+"\n")
        rid="resp_f"; created={"type":"response.created","response":{"id":rid}}
        usage={"input_tokens":5,"input_tokens_details":None,"output_tokens":5,"output_tokens_details":None,"total_tokens":10}
        done={"type":"response.completed","response":{"id":rid,"usage":usage}}
        tools=[t.get('name') or t.get('type') for t in j.get('tools',[])]
        LOG.write(f"OAI tools={tools}\n")
        if 'FLOOD' in json.dumps(last) and not hastr:
            item={"type":"message","role":"assistant","content":[{"type":"output_text","text":"flood done"}]}
            return self.stream([("response.created",created)],
             lambda t:("response.output_text.delta",{"type":"response.output_text.delta","delta":t}),
             [("response.output_item.done",{"type":"response.output_item.done","item":item}),("response.completed",done)])
        if 'RUNEDIT' in json.dumps(last) and not hastr:
            kinds={t.get('name'):t.get('type') for t in j.get('tools',[])}
            if kinds.get('apply_patch')=='custom':
                item={"type":"custom_tool_call","name":"apply_patch","input":PATCH,"call_id":"call_e"}
            elif 'apply_patch' in kinds:
                item={"type":"function_call","name":"apply_patch","arguments":json.dumps({"input":PATCH}),"call_id":"call_e"}
            else:
                # The CLI recognises this shell form and applies the patch itself.
                name='shell_command' if 'shell_command' in tools else 'exec_command'
                line="apply_patch <<'EOF'\n"+PATCH+"EOF\n"
                item={"type":"function_call","name":name,"arguments":json.dumps({'command':line} if name=='shell_command' else {'cmd':line}),"call_id":"call_e"}
            return self.sse([("response.created",created),("response.output_item.done",{"type":"response.output_item.done","item":item}),("response.completed",done)])
        if 'RUNTOOL' in json.dumps(last) and not hastr:
            name='shell_command' if 'shell_command' in tools else ('exec_command' if 'exec_command' in tools else 'shell')
            args={'command':TOOLCMD} if name=='shell_command' else ({'cmd':TOOLCMD} if name=='exec_command' else {'command':['bash','-lc',TOOLCMD]})
            item={"type":"function_call","name":name,"arguments":json.dumps(args),"call_id":"call_1"}
            ev=[("response.created",created),("response.output_item.done",{"type":"response.output_item.done","item":item}),("response.completed",done)]
        else:
            t="FAKE-CODEX-REPLY to: "+s[-60:]
            item={"type":"message","role":"assistant","content":[{"type":"output_text","text":t}]}
            ev=[("response.created",created),("response.output_item.done",{"type":"response.output_item.done","item":item}),("response.completed",done)]
        self.sse(ev)
ThreadingHTTPServer(('127.0.0.1',int(sys.argv[1])),H).serve_forever()
