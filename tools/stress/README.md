# OSSG stress test

Runs the real app (with its own settings folder under `D:\tmp\ossg-test`,
never your stream keys), a stand-in phone (`fake_phone.py`) and a local RTMP
server standing in for YouTube, then repeats every live feature and checks
the stream that arrives: gaps in video/audio, keyframe spacing, frame rate,
bitrate, plus errors in the app's log.

    python rig.py --rounds 15                # everything
    python rig.py --rounds 3 --only scenes,drops

Needs FFmpeg and the app built with the test control port (it opens only
when `PHONECAST_TEST_CONTROL=1`). Results: `out/<time>/report.json`.

Scenarios: scenes (Starting soon / BRB / Thanks and back via the action
clip), banner on/off, privacy cover, phone drop + reconnect (portrait first,
like the real phone), portrait flip, phone notification buttons, live
quality switch.
