"""A minimal language server for the restart tests.

- `fake/state` answers with this process's id and the documents it has open.
- `fake/exit` makes the process exit at once with code 3, without answering.
- `--slow-init` never answers `initialize`.
"""

import json
import os
import sys

slow_init = "--slow-init" in sys.argv
docs = {}


def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        name, _, value = line.decode().partition(":")
        if name.lower() == "content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


while True:
    msg = read()
    method, params, id_ = msg.get("method"), msg.get("params"), msg.get("id")
    if method == "initialize":
        if not slow_init:
            send({"jsonrpc": "2.0", "id": id_, "result": {"capabilities": {"textDocumentSync": 1}}})
    elif method == "textDocument/didOpen":
        d = params["textDocument"]
        docs[d["uri"]] = {"version": d["version"], "text": d["text"]}
    elif method == "textDocument/didChange":
        d = params["textDocument"]
        docs[d["uri"]] = {"version": d["version"], "text": params["contentChanges"][-1]["text"]}
    elif method == "textDocument/didClose":
        docs.pop(params["textDocument"]["uri"], None)
    elif method == "fake/state":
        send({"jsonrpc": "2.0", "id": id_, "result": {"pid": os.getpid(), "docs": docs}})
    elif method == "fake/exit":
        os._exit(3)
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": id_, "result": None})
    elif method == "exit":
        sys.exit(0)
