#!/usr/bin/env python3
"""Evaluate JS in Firefox via remote debugger protocol (raw socket, zero deps)."""
import socket, json, sys

def recv_msg(sock):
    buf = b""
    while b":" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            return None
        buf += chunk
    size_str, rest = buf.split(b":", 1)
    size = int(size_str)
    while len(rest) < size:
        rest += sock.recv(4096)
    return json.loads(rest[:size])

def send_msg(sock, data):
    raw = json.dumps(data)
    sock.sendall(f"{len(raw)}:{raw}".encode())

def main():
    expr = sys.argv[1] if len(sys.argv) > 1 else "1+1"
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(("127.0.0.1", 6000))
    recv_msg(s)  # hello

    # List tabs — skip zombie tabs, prefer the selected one
    send_msg(s, {"to": "root", "type": "listTabs"})
    resp = recv_msg(s)
    tab = None
    for t in resp["tabs"]:
        if not t.get("isZombieTab") and t.get("selected"):
            tab = t
            break
    if not tab:
        for t in resp["tabs"]:
            if not t.get("isZombieTab"):
                tab = t
                break
    if not tab:
        tab = resp["tabs"][0]
    tab_actor = tab["actor"]

    # Get target — drain until frame response
    send_msg(s, {"to": tab_actor, "type": "getTarget"})
    console_actor = None
    for _ in range(20):
        resp = recv_msg(s)
        if resp and "frame" in resp:
            console_actor = resp["frame"]["consoleActor"]
            break
    if not console_actor:
        print("No consoleActor found", file=sys.stderr)
        sys.exit(1)

    # Drain any remaining messages from getTarget
    s.settimeout(0.5)
    try:
        while True:
            recv_msg(s)
    except (socket.timeout, Exception):
        pass
    s.settimeout(10)

    # Evaluate
    send_msg(s, {
        "to": console_actor,
        "type": "evaluateJSAsync",
        "text": expr,
    })

    # Read: first the resultID ack, then the evaluationResult
    for _ in range(10):
        resp = recv_msg(s)
        if resp is None:
            break
        if resp.get("type") == "evaluationResult":
            result = resp.get("result", {})
            if isinstance(result, dict) and "type" in result:
                if result["type"] == "undefined":
                    print("undefined")
                else:
                    print(result.get("value", result.get("text", json.dumps(result))))
            elif isinstance(result, dict):
                print(json.dumps(result))
            else:
                print(result)
            s.close()
            return

    print("No evaluationResult received", file=sys.stderr)
    s.close()
    sys.exit(1)

if __name__ == "__main__":
    main()
