# SPDX-License-Identifier: CC0-1.0
"""Writes message.wav, the sound Inlet plays for a new notification.

The sound is synthesised here from two sine tones; it contains no recording and no
sample from any other source. Run with Python 3: python make_sounds.py
"""
import math
import struct
import wave

RATE = 48000
DURATION = 0.42
# (start in seconds, frequency in Hz, amplitude): A5, then E6.
NOTES = [(0.000, 880.0, 0.30), (0.075, 1318.51, 0.26)]


def sample(i: int, n: int) -> float:
    t = i / RATE
    v = 0.0
    for start, f, amp in NOTES:
        u = t - start
        if u < 0:
            continue
        # 4 ms attack, exponential decay, a quiet octave partial for a bell-like tone.
        env = min(1.0, u / 0.004) * math.exp(-u * 9.0)
        v += amp * env * (math.sin(2 * math.pi * f * u) + 0.18 * math.sin(4 * math.pi * f * u))
    # The last 20 ms fade to silence.
    tail = (n - i) / RATE
    if tail < 0.02:
        v *= tail / 0.02
    return max(-1.0, min(1.0, v))


def main() -> None:
    n = int(RATE * DURATION)
    frames = b"".join(
        struct.pack("<h", int(round(sample(i, n) * 32767 * 0.9))) for i in range(n)
    )
    with wave.open("message.wav", "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(frames)


if __name__ == "__main__":
    main()
