"""OSSG local stress test: runs the real app (its own settings folder, never
the user's keys), a stand-in phone, and a local RTMP server standing in for
YouTube, then repeats every live feature many times and checks the stream
that comes out for gaps, stalls, errors and platform-warning settings.

    python rig.py [--rounds 15] [--only scenes,banner,...] [--exe PATH]

Writes a report to tools/stress/out/report-<time>.json and prints a summary.
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from fake_phone import FakePhone  # noqa: E402

CONTROL = 27199
RTMP_PORT = 1935
OUT = os.path.join(HERE, "out")
TEST_ROOT = r"D:\tmp\ossg-test"
DEFAULT_EXE = r"D:\Personal\Streaming\PhoneCast-stream-fix\desktop\src-tauri\target\release\phonecast.exe"


def find_ffmpeg():
    for p in [
        os.path.expandvars(r"%LOCALAPPDATA%\Microsoft\WinGet\Links\ffmpeg.exe"),
        shutil.which("ffmpeg") or "",
    ]:
        if p and os.path.exists(p):
            return p
    raise SystemExit("ffmpeg not found")


def ffprobe_path(ffmpeg):
    p = os.path.join(os.path.dirname(ffmpeg), "ffprobe.exe")
    return p if os.path.exists(p) else (shutil.which("ffprobe") or "ffprobe")


# ---------------- the app under test ----------------

def prepare_appdata():
    """A copy of the user's look (banner, art, clips) without live.json."""
    src = os.path.join(os.environ["APPDATA"], "PhoneCast")
    dst = os.path.join(TEST_ROOT, "PhoneCast")
    shutil.rmtree(TEST_ROOT, ignore_errors=True)
    os.makedirs(dst)
    for name in ["banner.json", "banner.png", "settings.json", "source.json", "ai.json", "avatar.png", "scene_music.audio"]:
        if os.path.exists(os.path.join(src, name)):
            shutil.copy2(os.path.join(src, name), dst)
    for d in ["art"]:
        if os.path.isdir(os.path.join(src, d)):
            shutil.copytree(os.path.join(src, d), os.path.join(dst, d))
    bf = os.path.join(src, "banner_frames")
    if os.path.isdir(bf):
        cur = open(os.path.join(bf, "current.txt")).read().strip()
        os.makedirs(os.path.join(dst, "banner_frames"))
        shutil.copy2(os.path.join(bf, "current.txt"), os.path.join(dst, "banner_frames"))
        for f in ["plain.h264", "plain.png"]:
            if os.path.exists(os.path.join(bf, f)):
                shutil.copy2(os.path.join(bf, f), os.path.join(dst, "banner_frames"))
        if cur and os.path.isdir(os.path.join(bf, cur)):
            shutil.copytree(os.path.join(bf, cur), os.path.join(dst, "banner_frames", cur))
    return dst


class App:
    def __init__(self, exe):
        self.exe = exe
        self.proc = None
        self.sock = None
        self.file = None

    def start(self):
        env = dict(os.environ, APPDATA=TEST_ROOT, PHONECAST_TEST_CONTROL="1",
                   PHONECAST_RECORDINGS=os.path.join(TEST_ROOT, "recordings"))
        self.proc = subprocess.Popen([self.exe], env=env)
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                self.sock = socket.create_connection(("127.0.0.1", CONTROL), timeout=2)
                self.sock.settimeout(60)
                self.file = self.sock.makefile("rwb")
                return
            except OSError:
                time.sleep(0.5)
        raise RuntimeError("app control port never opened")

    def cmd(self, line):
        self.file.write((line + "\n").encode())
        self.file.flush()
        return json.loads(self.file.readline())

    def status(self):
        return self.cmd("status")

    def stop(self):
        try:
            self.cmd("live stop")
        except Exception:
            pass
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def log_path(self):
        d = os.path.join(TEST_ROOT, "PhoneCast", "logs")
        files = sorted(os.listdir(d)) if os.path.isdir(d) else []
        return os.path.join(d, files[-1]) if files else None


# ---------------- the stand-in platform ----------------

class Sink:
    """Local RTMP server; each publish is saved as its own segment."""

    def __init__(self, ffmpeg, folder):
        self.ffmpeg = ffmpeg
        self.folder = folder
        self.running = True
        self.segments = []
        self.proc = None
        os.makedirs(folder, exist_ok=True)
        threading.Thread(target=self._loop, daemon=True).start()

    def _loop(self):
        n = 0
        while self.running:
            n += 1
            path = os.path.join(self.folder, f"seg{n:03d}.flv")
            self.proc = subprocess.Popen(
                [self.ffmpeg, "-v", "error", "-y", "-listen", "1", "-timeout", "-1",
                 "-i", f"rtmp://127.0.0.1:{RTMP_PORT}/live2/test", "-c", "copy", "-f", "flv", path],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                # Above normal, like the app's own FFmpeg: the stand-in for
                # YouTube must not be starved by other work on this PC.
                creationflags=0x00008000 if os.name == "nt" else 0)
            _, err = self.proc.communicate()
            if os.path.exists(path) and os.path.getsize(path) > 0:
                self.segments.append({"path": path, "ended": time.time(), "err": err.decode(errors="replace")[-400:]})

    def stop(self):
        self.running = False
        if self.proc:
            self.proc.kill()


def analyze(ffprobe, path):
    """Gaps, keyframe spacing, frame rate and bitrate of one received segment."""
    out = subprocess.run(
        [ffprobe, "-v", "error", "-show_entries", "packet=stream_index,pts_time,dts_time,flags,size",
         "-show_entries", "stream=index,codec_type,codec_name,width,height,r_frame_rate,sample_rate",
         "-of", "json", path], capture_output=True, text=True).stdout
    try:
        j = json.loads(out)
    except json.JSONDecodeError:
        return {"path": path, "error": "unreadable"}
    streams = {s["index"]: s for s in j.get("streams", [])}
    vid = next((i for i, s in streams.items() if s["codec_type"] == "video"), None)
    aud = next((i for i, s in streams.items() if s["codec_type"] == "audio"), None)
    vt, at, keys, vbytes = [], [], [], 0
    for p in j.get("packets", []):
        t = float(p.get("dts_time") or p.get("pts_time") or 0)
        if p["stream_index"] == vid:
            vt.append(t)
            vbytes += int(p["size"])
            if "K" in p["flags"]:
                keys.append(t)
        elif p["stream_index"] == aud:
            at.append(t)

    def gaps(ts, limit):
        return [(round(a, 3), round(b - a, 3)) for a, b in zip(ts, ts[1:]) if b - a > limit]

    dur = (vt[-1] - vt[0]) if len(vt) > 1 else 0
    kint = [round(b - a, 2) for a, b in zip(keys, keys[1:])]
    v = streams.get(vid, {})
    return {
        "path": os.path.basename(path),
        "video": f'{v.get("codec_name")} {v.get("width")}x{v.get("height")}',
        "audio": streams.get(aud, {}).get("codec_name"),
        "seconds": round(dur, 1),
        "fps": round((len(vt) - 1) / dur, 2) if dur else 0,
        "mbps": round(vbytes * 8 / dur / 1e6, 2) if dur else 0,
        "video_gaps": gaps(vt, 0.1),
        "audio_gaps": gaps(at, 0.1),
        "max_keyframe_interval": max(kint) if kint else None,
        "backwards_dts": sum(1 for a, b in zip(vt, vt[1:]) if b < a),
    }


# ---------------- scenarios ----------------

def wait(pred, secs, step=0.2):
    end = time.time() + secs
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(step)
    return None


class Rig:
    def __init__(self, app, phone, rounds):
        self.app, self.phone, self.rounds = app, phone, rounds
        self.events = []
        self.recordings = []

    def check(self, name, ok, detail=""):
        self.events.append({"t": round(time.time(), 2), "check": name, "ok": bool(ok), "detail": detail})
        print(("  ok   " if ok else "  FAIL ") + name + (f"  ({detail})" if detail and not ok else ""), flush=True)
        return ok

    def scene_is(self, name, secs):
        return wait(lambda: self.app.status().get("scene") == name, secs)

    def scenes(self):
        for i in range(self.rounds):
            for name in ["starting", "brb", "thanks"]:
                r = self.app.cmd(f"scene {name}")
                self.check(f"scene {name} #{i+1} shown", r.get("ok") and self.scene_is(name, 3), r.get("error", ""))
                time.sleep(3)
                self.app.cmd("scene none")
                self.check(f"back to game from {name} #{i+1}", self.scene_is(None, 6))
                time.sleep(3)

    def banner(self):
        for i in range(self.rounds):
            for on in ["off", "on"]:
                self.app.cmd(f"banner {on}")
                time.sleep(2)
                self.check(f"banner {on} #{i+1}", self.app.status().get("bannerOn") == (on == "on"))

    def privacy(self):
        for i in range(self.rounds):
            self.phone.foreground("com.whatsapp", False)
            self.phone.hold = True
            self.check(f"privacy cover on #{i+1}", self.scene_is("brb", 3))
            time.sleep(3)
            self.phone.foreground("com.pubg.imobile", True)
            self.phone.hold = False
            self.check(f"privacy back to game #{i+1}", self.scene_is(None, 7))
            time.sleep(3)

    def drops(self):
        for i in range(self.rounds):
            self.phone.disconnect()
            self.check(f"phone drop -> Be right back #{i+1}", self.scene_is("brb", 4))
            time.sleep(4)
            # Like the real phone at reconnect: a portrait setup, then the game's.
            self.phone.shape = (872, 1920)
            self.phone.connect()
            time.sleep(0.3)
            self.phone.set_shape(1920, 872)
            self.phone.foreground("com.pubg.imobile", True)
            self.check(f"reconnect -> game #{i+1}", self.scene_is(None, 8))
            time.sleep(4)

    def portrait(self):
        for i in range(self.rounds):
            self.phone.set_shape(872, 1920)
            self.check(f"portrait -> Be right back #{i+1}", self.scene_is("brb", 3))
            time.sleep(3)
            self.phone.set_shape(1920, 872)
            self.check(f"landscape -> game #{i+1}", self.scene_is(None, 7))
            time.sleep(3)

    def phonecmd(self):
        for i in range(self.rounds):
            self.phone.command("brb")
            self.check(f"phone button BRB #{i+1}", self.scene_is("brb", 3))
            time.sleep(3)
            self.phone.command("game")
            self.check(f"phone button back to game #{i+1}", self.scene_is(None, 7))
            time.sleep(3)

    def record(self):
        """Record while scenes switch and the phone drops; the file must
        have no timestamp jumps or holes."""
        for i in range(max(2, self.rounds // 5)):
            r = self.app.cmd("record start")
            self.check(f"recording started #{i+1}", r.get("ok"), r.get("error", ""))
            time.sleep(4)
            self.app.cmd("scene brb")
            time.sleep(3)
            self.app.cmd("scene none")
            time.sleep(5)
            self.app.cmd("banner off")
            time.sleep(2)
            self.app.cmd("banner on")
            self.phone.disconnect()
            time.sleep(3)
            self.phone.shape = (1920, 872)
            self.phone.connect()
            self.phone.foreground("com.pubg.imobile", True)
            time.sleep(6)
            r = self.app.cmd("record stop")
            self.check(f"recording stopped #{i+1}", r.get("ok"), r.get("error", ""))
            time.sleep(2)
            self.recordings.append(r.get("path"))

    def popups(self):
        wait(lambda: len(self.app.cmd("popups").get("ready") or []) >= 2, 400, 2)
        ready = self.app.cmd("popups").get("ready") or []
        self.check("banner pop-ups made", len(ready) >= 2, str(ready))
        for i in range(self.rounds):
            name = ready[i % len(ready)] if ready else "subscribe"
            r = self.app.cmd(f"popup {name}")
            self.check(f"pop-up {name} asked #{i+1}", r.get("ok"), r.get("error", ""))
            # Next 5 s position + up to 1 s for the status to show it.
            on = wait(lambda: (self.app.status().get("bannerMoment") or {}).get("playing") == name, 8)
            self.check(f"pop-up {name} on stream #{i+1}", on)
            off = wait(lambda: not (self.app.status().get("bannerMoment") or {}).get("playing"), 8)
            self.check(f"pop-up {name} done #{i+1}", off)

    def switch(self):
        for i in range(max(3, self.rounds // 3)):
            r = self.app.cmd("live switch")
            self.check(f"apply settings live #{i+1}", r.get("ok"), r.get("error", ""))
            time.sleep(8)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=15)
    ap.add_argument("--only", default="popups,scenes,banner,privacy,drops,portrait,phonecmd,record,switch")
    ap.add_argument("--exe", default=DEFAULT_EXE)
    ap.add_argument("--preset", default="yt-1440p60-hevc")
    a = ap.parse_args()
    ffmpeg = find_ffmpeg()
    run_dir = os.path.join(OUT, time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(run_dir)
    prepare_appdata()
    sink = Sink(ffmpeg, run_dir)
    app = App(a.exe)
    phone = FakePhone(ffmpeg)
    phone.stream(1920, 872)
    phone.stream(872, 1920)
    rig = Rig(app, phone, a.rounds)
    started = time.time()
    try:
        app.start()
        app.cmd("privacy on")
        app.cmd("banner on")
        r = app.cmd("settings " + json.dumps({
            "destinations": [{"id": "youtube", "platform": "youtube", "enabled": True, "main": True,
                              "preset": a.preset, "url": f"rtmp://127.0.0.1:{RTMP_PORT}/live2", "key": "test"}],
            "framing": "banner"}))
        rig.check("test settings saved", r.get("ok"), r.get("error", ""))
        phone.connect()
        phone.foreground("com.pubg.imobile", True)
        rig.check("phone connected", wait(lambda: app.status().get("connected"), 15))
        for name in ["starting", "brb", "thanks", "stinger"]:
            app.cmd(f"prepare {name}")
            rig.check(f"{name} ready", wait(lambda: app.cmd(f"ready {name}").get("ready"), 90, 1))
        r = app.cmd("live start")
        rig.check("live started", r.get("ok"), r.get("error", ""))
        rig.check("live outputs running", wait(lambda: app.status().get("outputsRunning"), 20))
        time.sleep(5)
        for part in a.only.split(","):
            print(f"== {part}", flush=True)
            getattr(rig, part)()
        time.sleep(3)
    finally:
        app.stop()
        time.sleep(3)
        phone.disconnect()
        sink.stop()
        time.sleep(1)
    probe = ffprobe_path(ffmpeg)
    segs = [analyze(probe, s["path"]) for s in sink.segments]
    recs = []
    for p in rig.recordings:
        if p and os.path.exists(p):
            a = analyze(probe, p)
            # -c copy: the file's own timestamps (decoding re-times frames and
            # made up duplicates of its own).
            warn = subprocess.run([ffmpeg, "-v", "warning", "-i", p, "-c", "copy", "-f", "null", "-"],
                                  capture_output=True, text=True).stderr
            a["timestamp_warnings"] = sum(1 for l in warn.splitlines() if "monoton" in l or "Invalid" in l)
            recs.append(a)
    log = app.log_path()
    errors = []
    if log:
        for line in open(log, encoding="utf-8", errors="replace"):
            low = line.lower()
            if any(k in low for k in ["error", "panic", "stream ended", "reconnect attempt", "not ready", "had to be cut off", "impossible"]):
                errors.append(line.strip()[:240])
    # The stream itself must be perfect, not only the switches: these count
    # as failures like any check.
    for sg in segs:
        name = f"stream {sg.get('path')}"
        if sg.get("error"):
            rig.check(f"{name} readable", False, sg["error"])
            continue
        rig.check(f"{name}: no video gaps", not sg["video_gaps"], str(sg["video_gaps"][:5]))
        rig.check(f"{name}: no audio gaps", not sg["audio_gaps"], str(sg["audio_gaps"][:5]))
        rig.check(f"{name}: timestamps forward", sg["backwards_dts"] == 0, str(sg["backwards_dts"]))
        if sg["seconds"] >= 5:
            rig.check(f"{name}: 60 fps", sg["fps"] >= 59.5, str(sg["fps"]))
            rig.check(f"{name}: keyframes <= 2 s", (sg["max_keyframe_interval"] or 0) <= 2.05, str(sg["max_keyframe_interval"]))
    for r in recs:
        name = f"recording {r.get('path')}"
        rig.check(f"{name}: no video gaps", not r["video_gaps"], str(r["video_gaps"][:5]))
        rig.check(f"{name}: no audio gaps", not r["audio_gaps"], str(r["audio_gaps"][:5]))
        rig.check(f"{name}: timestamps forward", r["backwards_dts"] == 0 and r["timestamp_warnings"] == 0,
                  f'{r["backwards_dts"]} backwards, {r["timestamp_warnings"]} warnings')
    fails = [e for e in rig.events if not e["ok"]]
    report = {
        "seconds": round(time.time() - started),
        "checks": len(rig.events),
        "failed": fails,
        "segments": segs,
        "recordings": recs,
        "app_log_problems": errors[-80:],
        "phone": {"sent_frames": phone.sent_frames, "keyframe_asks": phone.keyframe_asks, "urgent_asks": phone.urgent_asks},
    }
    path = os.path.join(run_dir, "report.json")
    json.dump(report, open(path, "w"), indent=1)
    print(json.dumps({k: report[k] for k in ["seconds", "checks"]}), f"failed={len(fails)}")
    for s in segs:
        print("segment", json.dumps(s)[:600])
    for r in recs:
        print("recording", json.dumps(r)[:600])
    print(f"log problems: {len(errors)}")
    for e in errors[-25:]:
        print("  ", e)
    print("report:", path)


if __name__ == "__main__":
    main()
