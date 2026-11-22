"""Record both computers' screens during a real session and compose a desk-shaped video.

Desk (left to right): Ubuntu laptop | Windows laptop | shared monitor. The
monitor panel shows whichever computer the monitor is actually showing (from
Switchboard's own status, sampled 10x/s), with a short blank after each switch
to reflect the monitor re-syncing.

usage: python scripts/record_desk.py record <seconds> [delay]
       python scripts/record_desk.py compose [offset_ms]
"""
import json, os, socket, subprocess, sys, threading, time

FFMPEG = os.path.expandvars(r"%LOCALAPPDATA%\Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-9.1-full_build\bin\ffmpeg.exe")
OUT = os.path.join(os.environ.get("TEMP", "."), "sb-record")
HOST = "sbl"
FPS = 30
# Capture regions.
WIN_VIRT = dict(x=-1920, y=0, w=4480, h=1440)       # Windows virtual desktop
WIN_LAPTOP = dict(x=0, y=195, w=1920, h=1080)       # within the Windows capture
WIN_SHARED = dict(x=1920, y=0, w=2560, h=1440)
LIN_FULL = dict(w=5120, h=1600)
LIN_LAPTOP = dict(x=0, y=0, w=2560, h=1600)
LIN_SHARED = dict(x=2560, y=0, w=2560, h=1440)
WIN_INPUT = 0x0F  # DisplayPort: monitor shows Windows


def ipc(req):
    tok = open(os.path.join(os.environ["APPDATA"], "Switchboard", "ipc.token")).read().strip()
    s = socket.create_connection(("127.0.0.1", 47803), timeout=2)
    s.sendall((tok + "\n" + json.dumps(req) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        buf += s.recv(65536)
    return json.loads(buf)


def clock_offset():
    """remote_clock - local_clock, seconds. One persistent SSH session answers
    timestamp pings, so each sample costs one network round trip (a few ms)."""
    p = subprocess.Popen(["ssh", HOST, "python3", "-u", "-c",
                          "'import sys,time; [print(repr(time.time()), flush=True) for _ in sys.stdin]'"],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    best = None
    for _ in range(25):
        t1 = time.time()
        p.stdin.write("x" + chr(10)); p.stdin.flush()
        r = float(p.stdout.readline())
        t2 = time.time()
        if best is None or (t2 - t1) < best[1]:
            best = (r - (t1 + t2) / 2, t2 - t1)
    p.stdin.close(); p.wait()
    return best


def record(seconds, delay=8.0):
    os.makedirs(OUT, exist_ok=True)
    off, rtt = clock_offset()
    print(f"clock offset {off*1000:+.1f} ms (rtt {rtt*1000:.0f} ms)")
    t0 = time.time() + delay
    remote_t0 = t0 + off
    lin_cmd = (f"sleep $(python3 -c 'import time; print(max(0, {remote_t0:.3f} - time.time()))'); "
               f"date +%s.%N > /tmp/sb-lin.start; "
               f"DISPLAY=:1 XAUTHORITY=/run/user/1001/gdm/Xauthority ffmpeg -loglevel error -y -f x11grab -draw_mouse 1 -framerate {FPS} "
               f"-video_size {LIN_FULL['w']}x{LIN_FULL['h']} -i :1.0 -t {seconds} -c:v libx264 -preset ultrafast -crf 18 /tmp/sb-lin.mkv")
    lin = subprocess.Popen(["ssh", HOST, "bash", "-c", f'"{lin_cmd}"'])
    # Monitor/pointer timeline.
    timeline = []
    stop = threading.Event()

    def poll():
        while not stop.is_set():
            try:
                s = ipc({"cmd": "status"})
                timeline.append((time.time() - t0, s["monitor"].get("observed"), s["mode"]))
            except Exception:
                pass
            time.sleep(0.1)

    threading.Thread(target=poll, daemon=True).start()
    while time.time() < t0:
        time.sleep(0.002)
    win_start = time.time()
    subprocess.run([FFMPEG, "-loglevel", "error", "-y", "-f", "gdigrab", "-draw_mouse", "1", "-framerate", str(FPS),
                    "-offset_x", str(WIN_VIRT["x"]), "-offset_y", str(WIN_VIRT["y"]),
                    "-video_size", f"{WIN_VIRT['w']}x{WIN_VIRT['h']}", "-i", "desktop", "-t", str(seconds),
                    "-c:v", "libx264", "-preset", "ultrafast", "-crf", "18", os.path.join(OUT, "win.mkv")])
    stop.set()
    lin.wait()
    lin_start = float(subprocess.run(["ssh", HOST, "cat /tmp/sb-lin.start"], capture_output=True, text=True).stdout) - off
    subprocess.run(["scp", "-q", f"{HOST}:/tmp/sb-lin.mkv", os.path.join(OUT, "lin.mkv")], check=True)
    meta = {"win_start": win_start - t0, "lin_start": lin_start - t0, "timeline": timeline, "seconds": seconds}
    json.dump(meta, open(os.path.join(OUT, "meta.json"), "w"))
    print(f"recorded; windows started {meta['win_start']*1000:+.0f} ms, linux {meta['lin_start']*1000:+.0f} ms after t0; {len(timeline)} status samples")


def compose(extra_offset_ms=0.0):
    meta = json.load(open(os.path.join(OUT, "meta.json")))
    # Shift the Linux stream so both share the Windows clock.
    lin_shift = meta["lin_start"] - meta["win_start"] + extra_offset_ms / 1000.0
    # Monitor segments: (start, end, showing_windows)
    segs, cur, start = [], None, 0.0
    for t, obs, _ in meta["timeline"]:
        t -= meta["win_start"]
        win = (obs is None) or ((obs & 0xFF) == WIN_INPUT)
        if cur is None:
            cur, start = win, 0.0
        elif win != cur:
            segs.append((start, t, cur))
            cur, start = win, t
    segs.append((start, meta["seconds"] + 1, cur if cur is not None else True))
    BLANK = 1.4  # monitor re-sync after a switch (observed 1-3 s)
    H_LAP, H_MON = 400, 520
    def size(r, h):
        return (round(r["w"] * h / r["h"] / 2) * 2, h)
    lw, lh = size(LIN_LAPTOP, H_LAP)
    ww, wh = size(WIN_LAPTOP, H_LAP)
    mw, mh = size(WIN_SHARED, H_MON)
    gap, pad_top, pad_bot = 48, 70, 90
    W = 48 + lw + gap + ww + gap + mw + 48
    H = pad_top + H_MON + pad_bot
    base_y = pad_top + H_MON
    xl, xw, xm = 48, 48 + lw + gap, 48 + lw + gap + ww + gap
    font = "C\\:/Windows/Fonts/segoeui.ttf"
    fontb = "C\\:/Windows/Fonts/seguisb.ttf"
    crop = lambda r: f"crop={r['w']}:{r['h']}:{r['x']}:{r['y']}"
    f = []
    f.append(f"color=c=0x18202b:s={W}x{H}:r={FPS}[bg]")
    f.append(f"[1:v]setpts=PTS-STARTPTS+{max(0, lin_shift):.3f}/TB,split=2[l1][l2]")
    f.append(f"[0:v]setpts=PTS-STARTPTS+{max(0, -lin_shift):.3f}/TB,split=2[w1][w2]")
    f.append(f"[l1]{crop(LIN_LAPTOP)},scale={lw}:{lh}[ul]")
    f.append(f"[l2]{crop(LIN_SHARED)},scale={mw}:{mh}[um]")
    f.append(f"[w1]{crop(WIN_LAPTOP)},scale={ww}:{wh}[wl]")
    f.append(f"[w2]{crop(WIN_SHARED)},scale={mw}:{mh}[wm]")
    win_on = "+".join(f"between(t,{a + (BLANK if i else 0):.3f},{b:.3f})" for i, (a, b, w) in enumerate(segs) if w) or "0"
    ubu_on = "+".join(f"between(t,{a + (BLANK if i else 0):.3f},{b:.3f})" for i, (a, b, w) in enumerate(segs) if not w) or "0"
    f.append(f"[bg][ul]overlay={xl}:{base_y - lh}:shortest=0[a]")
    f.append(f"[a][wl]overlay={xw}:{base_y - wh}[b]")
    f.append(f"[b]drawbox=x={xm}:y={base_y - mh}:w={mw}:h={mh}:color=black:t=fill[c]")
    f.append(f"[c][wm]overlay={xm}:{base_y - mh}:enable='{win_on}'[d]")
    f.append(f"[d][um]overlay={xm}:{base_y - mh}:enable='{ubu_on}'[e]")
    lab = lambda x, w, text, col="0xe9eef2", extra="": f"drawtext=fontfile='{fontb}':text='{text}':fontcolor={col}:fontsize=26:x={x}+({w}-text_w)/2:y={base_y + 22}{extra}"
    sub = lambda x, w, text, col, en: f"drawtext=fontfile='{font}':text='{text}':fontcolor={col}:fontsize=21:x={x}+({w}-text_w)/2:y={base_y + 56}:enable='{en}'"
    f.append(f"[e]{lab(xl, lw, 'Ubuntu laptop')},{lab(xw, ww, 'Windows laptop')},{lab(xm, mw, 'Shared monitor')},"
             f"{sub(xm, mw, 'showing Windows', '0xe0645a', win_on)},{sub(xm, mw, 'showing Ubuntu', '0x3db2a4', ubu_on)},"
             f"drawtext=fontfile='{fontb}':text='One mouse. Two computers. The monitor follows.':fontcolor=0xc29a4a:fontsize=30:x=48:y=22[out]")
    cmd = [FFMPEG, "-loglevel", "error", "-y", "-i", os.path.join(OUT, "win.mkv"), "-i", os.path.join(OUT, "lin.mkv"),
           "-filter_complex", ";".join(f), "-map", "[out]", "-t", str(meta["seconds"] - 0.5),
           "-c:v", "libx264", "-preset", "slow", "-crf", "20", "-pix_fmt", "yuv420p", "-movflags", "+faststart", os.path.join(OUT, "switchboard-desk.mp4")]
    subprocess.run(cmd, check=True)
    print("segments:", [(round(a, 2), round(b, 2), "win" if w else "ubu") for a, b, w in segs])
    print("wrote", os.path.join(OUT, "switchboard-desk.mp4"), f"({W}x{H})")


if __name__ == "__main__":
    if sys.argv[1] == "record":
        record(float(sys.argv[2]), float(sys.argv[3]) if len(sys.argv) > 3 else 8.0)
    else:
        compose(float(sys.argv[2]) if len(sys.argv) > 2 else 0.0)
