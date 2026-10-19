"""A stand-in phone for OSSG's stress tests: speaks the phone protocol
(PROTOCOL.md) to the PC app on 127.0.0.1:27183 with real H.264 video and
48 kHz audio, paced in real time, answering the PC's clock pings.

Used by rig.py; can also run alone: `python fake_phone.py 30` (seconds).
"""
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time

PORT = 27183
VIDEO_CONFIG, VIDEO_FRAME, AUDIO_CONFIG, AUDIO_PCM, STATS, PONG = 1, 2, 3, 4, 5, 6
FOREGROUND_APP = 8
PHONE_COMMAND = 9
PING, REQUEST_KEYFRAME = 0x83, 0x82
HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, ".cache")


def now_us():
    return time.monotonic_ns() // 1000


def nal_units(es):
    """Annex B -> list of NAL unit bytes (without start codes)."""
    out, i, n = [], 0, len(es)
    starts = []
    while i < n - 3:
        if es[i] == 0 and es[i + 1] == 0 and (es[i + 2] == 1 or (es[i + 2] == 0 and i + 3 < n and es[i + 3] == 1)):
            sc = 3 if es[i + 2] == 1 else 4
            starts.append((i, i + sc))
            i += sc
        else:
            i += 1
    for k, (s, b) in enumerate(starts):
        e = starts[k + 1][0] if k + 1 < len(starts) else n
        out.append(es[b:e])
    return out


def make_stream(ffmpeg, w, h, secs=20, key_every=0.5):
    """(csd, frames) for a w x h 60 fps test picture: frames are access units
    like the phone's encoder gives (slices only; SPS/PPS go in the config)."""
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, f"phone_{w}x{h}_{key_every}.h264")
    if not os.path.exists(path):
        subprocess.run([
            ffmpeg, "-v", "error", "-y", "-f", "lavfi", "-i", f"testsrc2=s={w}x{h}:r=60:d={secs}",
            "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-b:v", "12M",
            "-bf", "0", "-g", "600", "-force_key_frames", f"expr:gte(t,n_forced*{key_every})",
            "-pix_fmt", "yuv420p", "-color_range", "tv", "-colorspace", "bt709",
            "-color_primaries", "bt709", "-color_trc", "bt709", "-f", "h264", path,
        ], check=True)
    es = open(path, "rb").read()
    csd, frames, cur, key = b"", [], [], False
    sc = b"\x00\x00\x00\x01"
    for nal in nal_units(es):
        t = nal[0] & 0x1F
        if t in (7, 8):
            if len(csd) < 200 and not frames:
                csd += sc + nal
            continue
        if t in (9, 6):  # AUD, SEI
            continue
        if t in (1, 5):
            first_mb = nal[1] & 0x80  # first_mb_in_slice == 0 -> new picture
            if first_mb and cur:
                frames.append((key, b"".join(cur)))
                cur, key = [], False
            cur.append(sc + nal)
            key = key or t == 5
    if cur:
        frames.append((key, b"".join(cur)))
    return csd, frames


class FakePhone:
    def __init__(self, ffmpeg, name="OSSG test phone"):
        self.ffmpeg = ffmpeg
        self.name = name
        self.sock = None
        self.lock = threading.Lock()
        self.running = False
        self.streams = {}
        self.shape = (1920, 872)
        self.hold = False  # privacy: send no picture
        self.keyframe_asks = 0
        self.urgent_asks = 0
        self.sent_frames = 0

    def stream(self, w, h):
        if (w, h) not in self.streams:
            self.streams[(w, h)] = make_stream(self.ffmpeg, w, h)
        return self.streams[(w, h)]

    def send(self, ty, payload=b"", flags=0, pts=None):
        pkt = struct.pack(">BBqI", ty, flags, now_us() if pts is None else pts, len(payload)) + payload
        with self.lock:
            if self.sock:
                self.sock.sendall(pkt)

    def connect(self):
        s = socket.create_connection(("127.0.0.1", PORT), timeout=5)
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.settimeout(None)
        name = self.name.encode()
        s.sendall(b"PCST" + struct.pack(">BH", 1, len(name)) + name)
        self.sock = s
        self.running = True
        self.send_config()
        self.send(AUDIO_CONFIG, struct.pack(">IBB", 48000, 2, 1))
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._video, daemon=True).start()
        threading.Thread(target=self._audio, daemon=True).start()

    def send_config(self):
        w, h = self.shape
        csd, _ = self.stream(w, h)
        self.send(VIDEO_CONFIG, struct.pack(">BHH", 1, w, h) + csd)

    def set_shape(self, w, h):
        self.stream(w, h)
        self.shape = (w, h)
        self.send_config()

    def foreground(self, pkg, game):
        self.send(FOREGROUND_APP, json.dumps({"pkg": pkg, "label": pkg, "game": game}).encode())

    def command(self, cmd):
        """A notification button (brb / game / clip)."""
        self.send(PHONE_COMMAND, cmd.encode())

    def disconnect(self):
        self.running = False
        with self.lock:
            if self.sock:
                try:
                    self.sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                self.sock.close()
                self.sock = None

    def _read(self):
        f = self.sock.makefile("rb")
        try:
            while self.running:
                hdr = f.read(14)
                if len(hdr) < 14:
                    break
                ty, _flags, _pts, n = struct.unpack(">BBqI", hdr)
                payload = f.read(n) if n else b""
                if ty == PING:
                    pc = struct.unpack(">q", payload[:8])[0]
                    self.send(PONG, struct.pack(">qq", pc, now_us()), pts=0)
                elif ty == REQUEST_KEYFRAME:
                    self.keyframe_asks += 1
                    if payload[:1] == b"\x01":
                        self.urgent_asks += 1
        except OSError:
            pass

    def _video(self):
        period = 1 / 60
        t0 = time.monotonic()
        t0_us = now_us()
        n = 0
        shape = None
        i = 0
        wait_key = True
        while self.running:
            if shape != self.shape:
                shape = self.shape
                _, frames = self.stream(*shape)
                i = 0
                wait_key = True
            key, data = frames[i % len(frames)]
            i += 1
            if not self.hold and not (wait_key and not key):
                wait_key = False
                try:
                    self.send(VIDEO_FRAME, data, flags=1 if key else 0, pts=t0_us + int(n * 1_000_000 / 60))
                    self.sent_frames += 1
                except OSError:
                    break
            elif self.hold:
                wait_key = True
            n += 1
            d = t0 + n * period - time.monotonic()
            if d > 0:
                time.sleep(d)

    def _audio(self):
        import math
        chunk = 480  # 10 ms
        t0 = time.monotonic()
        t0_us = now_us()
        n = 0
        ph = 0.0
        while self.running:
            buf = bytearray()
            for _ in range(chunk):
                v = int(6000 * math.sin(ph))
                ph += 2 * math.pi * 440 / 48000
                buf += struct.pack("<hh", v, v)
            try:
                self.send(AUDIO_PCM, bytes(buf), pts=t0_us + n * 10_000)
            except OSError:
                break
            n += 1
            d = t0 + n * 0.01 - time.monotonic()
            if d > 0:
                time.sleep(d)


if __name__ == "__main__":
    from rig import find_ffmpeg
    p = FakePhone(find_ffmpeg())
    p.connect()
    time.sleep(float(sys.argv[1]) if len(sys.argv) > 1 else 30)
    p.disconnect()
