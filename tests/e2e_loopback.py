#!/usr/bin/env python3
"""End-to-end loopback test: two Switchboard daemons on one machine.

A = source (synthetic capture), B = sink (recording injector, simulated
1920x1080 screen). Exercises pairing, edge crossing, encrypted UDP input,
button ordering, wheel, return-edge Leave, packet-loss tolerance, and
disconnect recovery. Never touches the real mouse or monitor.

usage: python tests/e2e_loopback.py <path-to-switchboard-binary> [drop_every_n]
"""
import json, os, socket, subprocess, sys, tempfile, time

BIN = sys.argv[1]
DROP = sys.argv[2] if len(sys.argv) > 2 else "0"
root = tempfile.mkdtemp(prefix="sb-e2e-")


def mk(name, port, ipc, extra):
    home = os.path.join(root, name)
    os.makedirs(home)
    with open(os.path.join(home, "switchboard.toml"), "w") as f:
        f.write(f'name = "{name}"\nport = {port}\ndiscovery = false\n[monitor]\nenabled = false\n')
    env = dict(os.environ, SWITCHBOARD_HOME=home, SWITCHBOARD_IPC_PORT=str(ipc), SWITCHBOARD_TEST="1",
               SWITCHBOARD_INJECT="null", SWITCHBOARD_CAPTURE="none", **extra)
    log = open(os.path.join(home, "out.log"), "w")
    p = subprocess.Popen([BIN, "run"], env=env, stderr=log, stdout=log)
    return {"home": home, "ipc": ipc, "env": env, "proc": p, "log": log.name}


def ipc(n, req):
    tok = open(os.path.join(n["home"], "ipc.token")).read().strip()
    s = socket.create_connection(("127.0.0.1", n["ipc"]), timeout=10)
    s.sendall((tok + "\n" + json.dumps(req) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        c = s.recv(65536)
        if not c:
            break
        buf += c
    s.close()
    return json.loads(buf)


def wait(pred, what, t=5.0):
    end = time.time() + t
    while time.time() < end:
        try:
            if pred():
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise AssertionError("timeout waiting for " + what)


ok = True
def check(cond, msg):
    global ok
    print(("PASS " if cond else "FAIL ") + msg)
    ok &= bool(cond)


A = mk("alpha", 48801, 48803, {"SWITCHBOARD_TEST_DROP": DROP})
B = mk("beta", 48901, 48903, {})
try:
    wait(lambda: ipc(A, {"cmd": "status"}) and ipc(B, {"cmd": "status"}), "daemons up")
    t0 = time.time()
    print(ipc(A, {"cmd": "pair", "address": "127.0.0.1:48901"}))
    wait(lambda: any(not p["trusted"] for p in ipc(A, {"cmd": "status"})["peers"]), "pending pairing on A")
    wait(lambda: any(not p["trusted"] for p in ipc(B, {"cmd": "status"})["peers"]), "pending pairing on B")
    ca = ipc(A, {"cmd": "status"})["peers"][0]["pairing_code"]
    cb = ipc(B, {"cmd": "status"})["peers"][0]["pairing_code"]
    check(ca == cb, f"pairing codes match ({ca} / {cb})")
    print(ipc(A, {"cmd": "confirm"}))
    print(ipc(B, {"cmd": "confirm"}))
    wait(lambda: ipc(A, {"cmd": "status"})["peers"][0]["trusted"] and ipc(B, {"cmd": "status"})["peers"][0]["trusted"], "trusted")
    check(True, f"paired in {time.time()-t0:.2f}s")
    ipc(A, {"cmd": "link", "edge": "right", "peer": "beta"})
    ipc(B, {"cmd": "link", "edge": "left", "peer": "alpha"})

    # Clipboard text: pushed on copy, no echo back.
    msg = "héllo from alpha" + chr(10) + "line 2"
    ipc(A, {"cmd": "clip_set", "text": msg})
    wait(lambda: ipc(B, {"cmd": "clip_get"}).get("text") == msg, "text on B", 5)
    check(True, "clipboard text A->B")
    time.sleep(1.0)
    check(ipc(A, {"cmd": "clip_get"})["count"] == 1, "no clipboard echo back to A")
    ipc(B, {"cmd": "clip_set", "text": "reply from beta"})
    wait(lambda: ipc(A, {"cmd": "clip_get"}).get("text") == "reply from beta", "text on A", 5)
    check(True, "clipboard text B->A")

    # Files: offered on copy, transferred when the pointer moves to B.
    src = os.path.join(root, "files")
    os.makedirs(os.path.join(src, "folder", "sub"))
    payload = os.urandom(300_000)
    open(os.path.join(src, "folder", "sub", "big.bin"), "wb").write(payload)
    open(os.path.join(src, "folder", "a.txt"), "w").write("A")
    open(os.path.join(src, "note.txt"), "w").write("note")
    ipc(A, {"cmd": "clip_set", "files": [os.path.join(src, "folder"), os.path.join(src, "note.txt")]})
    time.sleep(0.5)
    check("files" not in ipc(B, {"cmd": "clip_get"}), "files not sent before the pointer moves")

    # Edge cross: cursor at right edge, push right.
    ipc(A, {"cmd": "synth", "x": 1919, "y": 540, "events": [["m", 4, 0]]})
    wait(lambda: ipc(A, {"cmd": "status"})["mode"].startswith("controlling"), "A controlling")
    wait(lambda: ipc(B, {"cmd": "status"})["mode"].startswith("controlled"), "B hosted")
    wait(lambda: "files" in ipc(B, {"cmd": "clip_get"}), "files on B", 10)
    got = ipc(B, {"cmd": "clip_get"})["files"]
    names = sorted(os.path.basename(f) for f in got)
    check(names == ["folder", "note.txt"], f"received top-level items {names}")
    fdir = [f for f in got if f.endswith("folder")][0]
    check(open(os.path.join(fdir, "sub", "big.bin"), "rb").read() == payload, "300 KB file byte-identical")
    check(open(os.path.join(fdir, "a.txt")).read() == "A", "folder structure preserved")
    import shutil; shutil.rmtree(os.path.dirname(fdir), ignore_errors=True)
    r = ipc(B, {"cmd": "recorded"})
    check((r["x"], r["y"]) == (1, 540), f"entry point on B left edge = {(r['x'], r['y'])}")

    # Motion, click-drag-release, wheel. 200 small moves like a 1000 Hz mouse.
    evs = [["m", 1, 0]] * 100 + [["d", 0]] + [["m", 0, 1]] * 50 + [["u", 0]] + [["w", 120, 0], ["w", -240, 0], ["w", 0, 120]]
    evs += [["d", 1], ["u", 1], ["d", 3], ["u", 3]]
    for i in range(0, len(evs), 20):
        ipc(A, {"cmd": "synth", "events": evs[i:i+20]})
    wait(lambda: ipc(B, {"cmd": "recorded"})["log"][-1:] == ["Back up @101,590"], "events applied", 5)
    r = ipc(B, {"cmd": "recorded"})
    check((r["x"], r["y"]) == (101, 590), f"cursor after motion = {(r['x'], r['y'])} (expect 101,590)")
    check(r["log"][:2] == ["Left down @101,540", "Left up @101,590"], f"click/drag positions exact: {r['log'][:2]}")
    check(r["log"][2:] == ["Right down @101,590", "Right up @101,590", "Back down @101,590", "Back up @101,590"], f"other buttons: {r['log'][2:]}")
    check((r["wheel_v"], r["wheel_h"]) == (-120, 120), f"wheel v/h = {(r['wheel_v'], r['wheel_h'])}")
    check(r["buttons"] == 0, "no buttons left pressed")

    # Keyboard follows the pointer: type "hi" (KEY_H=35, KEY_I=23) with shift held on 'h'.
    ipc(A, {"cmd": "synth", "events": [["k", 42, True], ["k", 35, True], ["k", 35, False], ["k", 42, False], ["k", 23, True], ["k", 23, False]]})
    wait(lambda: ipc(B, {"cmd": "recorded"})["log"][-1:] == ["key 23 up"], "keys applied", 5)
    keys = [l for l in ipc(B, {"cmd": "recorded"})["log"] if l.startswith("key")]
    check(keys == ["key 42 down", "key 35 down", "key 35 up", "key 42 up", "key 23 down", "key 23 up"], f"keystrokes in order: {keys}")
    check(ipc(A, {"cmd": "status"})["keyboard_to"] == "beta", "A keyboard forwarding to beta")
    # Hold a key, then return the pointer: B must release it.
    ipc(A, {"cmd": "synth", "events": [["k", 30, True]]})
    wait(lambda: 30 in ipc(B, {"cmd": "recorded"})["keys"], "A key held on B")

    # Return via B's left edge.
    ipc(A, {"cmd": "synth", "events": [["m", -50, 0]] * 3})
    wait(lambda: ipc(A, {"cmd": "status"})["mode"] == "local", "A local again")
    check(ipc(B, {"cmd": "status"})["mode"] == "local", "B local after leave")
    wait(lambda: ipc(B, {"cmd": "recorded"})["keys"] == [], "held key released on B after pointer left", 3)
    check(True, "held key released on B when pointer returned")
    check(ipc(A, {"cmd": "status"})["keyboard_to"] is None, "A keyboard local again")

    # Stuck-button recovery: press on B then kill A.
    ipc(A, {"cmd": "synth", "x": 1919, "y": 300, "events": [["m", 4, 0]]})
    check(ipc(A, {"cmd": "status"})["mode"] == "local", "edge disarmed right after return (no bounce)")
    ipc(A, {"cmd": "synth", "x": 1800, "y": 300, "events": [["m", -1, 0]]})  # move off the edge: re-arms
    ipc(A, {"cmd": "synth", "x": 1919, "y": 300, "events": [["m", 4, 0]]})
    wait(lambda: ipc(B, {"cmd": "status"})["mode"].startswith("controlled"), "B hosted again")
    ipc(A, {"cmd": "synth", "events": [["d", 2]]})
    wait(lambda: ipc(B, {"cmd": "recorded"})["buttons"] == 4, "middle held on B")
    t_kill = time.time()
    A["proc"].kill()
    wait(lambda: ipc(B, {"cmd": "recorded"})["buttons"] == 0, "B released buttons after A died", 3)
    check(True, f"B released held button {time.time()-t_kill:.2f}s after source died")
    wait(lambda: ipc(B, {"cmd": "status"})["mode"] == "local", "B local after A died", 8)
    check(True, f"B back to local {time.time()-t_kill:.2f}s after source died")

    st = ipc(B, {"cmd": "status"})["stats"]
    print("B stats:", json.dumps({k: st[k] for k in ("sink_received", "sink_gaps", "sink_inject_us_p50", "udp_rejected")}))
finally:
    for n in (A, B):
        try:
            n["proc"].kill()
        except Exception:
            pass
print("RESULT:", "ALL PASS" if ok else "FAILURES", f"(drop every {DROP})")
print("logs in", root)
sys.exit(0 if ok else 1)
