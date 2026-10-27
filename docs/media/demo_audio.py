#!/usr/bin/env python3
"""An original, deterministic instrumental bed for the amm demo film.

Only NumPy and the standard library are used: no samples, soundfonts or network.
Writes 48 kHz stereo 16-bit PCM. The arrangement follows the film's chapters:
a warm pad and a slow bass for the opening, a ticking sequence once the briefs
are written (the workers start), a soft beat from the worktrees chapter, a
brighter bell motif while branches merge, and only the pad again for the title.

    python docs/media/demo_audio.py --output score.wav --duration 54
"""

from __future__ import annotations

import argparse
import math
import wave
from pathlib import Path

import numpy as np

RATE = 48_000
BPM = 96
BEAT = 60 / BPM
BAR = BEAT * 4
# Am9 · Fmaj7 · Cmaj7 · G6 as MIDI notes, one chord per bar; the first note is the root.
CHORDS = ((57, 60, 64, 71), (53, 57, 60, 64), (48, 52, 55, 59), (55, 59, 62, 64))
SEQ_IN, BEAT_IN, BELLS_IN, BELLS_OUT, BEAT_OUT = 5.0, 11.0, 35.0, 46.0, 50.0


def hz(note):
    return 440 * 2 ** ((note - 69) / 12)


def envelope(n, attack, release):
    t = np.arange(n) / RATE
    a = np.clip(t / attack, 0, 1) if attack else np.ones(n)
    total = n / RATE
    r = np.clip((total - t) / release, 0, 1) if release else np.ones(n)
    return a * r


def soft(signal, width=24):
    """A gentle lowpass: a short moving average, applied twice."""
    kernel = np.ones(width) / width
    for _ in range(2):
        signal = np.convolve(signal, kernel, mode="same")
    return signal


def chord_at(time):
    return CHORDS[int(time // BAR) % len(CHORDS)]


def pad(duration):
    """Each bar's chord swells in over the previous one, detuned left and right, with a slow shimmer."""
    n = int(duration * RATE)
    left, right = np.zeros(n), np.zeros(n)
    for bar in range(math.ceil(duration / BAR)):
        chord = CHORDS[bar % len(CHORDS)]
        start = int(bar * BAR * RATE)
        length = min(n - start, int((BAR + 1.4) * RATE))
        if length <= 0:
            break
        t = np.arange(length) / RATE
        env = envelope(length, 1.1, 1.3)
        shimmer = 1 + 0.12 * np.sin(2 * np.pi * 0.35 * t + bar)
        for i, note in enumerate(chord):
            f = hz(note)
            for channel, detune in ((left, 0.997), (right, 1.003)):
                w = f * detune
                tone = np.sin(2 * np.pi * w * t) + 0.28 * np.sin(2 * np.pi * 2 * w * t + 0.5)
                tone += 0.08 * np.sin(2 * np.pi * 3 * w * t + 1.1)
                channel[start : start + length] += tone * env * shimmer * (0.09 if i else 0.06)
    return soft(left), soft(right)


def bass(duration):
    """A round sine bass on the root, two notes per bar."""
    n = int(duration * RATE)
    out = np.zeros(n)
    time = 0.0
    k = 0
    while time < duration:
        root = chord_at(time)[0] - 24
        note = root + (7 if k % 2 else 0)
        start = int(time * RATE)
        length = min(int(BAR / 2 * RATE), n - start)
        if length <= 0:
            break
        t = np.arange(length) / RATE
        tone = np.sin(2 * np.pi * hz(note) * t) + 0.2 * np.sin(2 * np.pi * 2 * hz(note) * t)
        out[start : start + length] += tone * envelope(length, 0.03, 0.4) * 0.22
        k += 1
        time += BAR / 2
    return out


def blip(f, length):
    """A short square-ish tick, filtered: the sound of tool calls going by."""
    t = np.arange(length) / RATE
    square = np.tanh(4 * np.sin(2 * np.pi * f * t))
    return soft(square, 10) * np.exp(-t * 16) * np.clip(t / 0.003, 0, 1)


def sequence(duration):
    """Sixteenth-note arpeggio that runs while the workers work."""
    n = int(duration * RATE)
    left, right = np.zeros(n), np.zeros(n)
    step = BEAT / 4
    pattern = (0, 2, 1, 3, 2, 1, 3, 2)
    accents = (1.0, 0.5, 0.7, 0.5)
    k = 0
    time = SEQ_IN
    while time < min(duration, BEAT_OUT):
        chord = chord_at(time)
        index = pattern[k % len(pattern)]
        note = chord[index] + 12
        start = int(time * RATE)
        length = min(int(0.22 * RATE), n - start)
        if length <= 0:
            break
        swell = np.clip((time - SEQ_IN) / 4, 0, 1)
        tone = blip(hz(note), length) * 0.07 * accents[k % 4] * swell
        pan = 0.3 + 0.4 * ((k % 8) / 7)
        left[start : start + length] += tone * (1 - pan)
        right[start : start + length] += tone * pan
        k += 1
        time += step
    return left, right


def bell(f, length):
    t = np.arange(length) / RATE
    tone = np.sin(2 * np.pi * f * t) + 0.4 * np.sin(2 * np.pi * 2.76 * f * t) * np.exp(-t * 6)
    return tone * np.exp(-t * 2.4) * np.clip(t / 0.002, 0, 1)


def bells(duration):
    """A rising three-note motif, once per bar: one per merge."""
    n = int(duration * RATE)
    left, right = np.zeros(n), np.zeros(n)
    time = BELLS_IN
    while time < min(duration, BELLS_OUT):
        chord = chord_at(time)
        for j, index in enumerate((1, 2, 3)):
            at = time + j * BEAT / 2
            start = int(at * RATE)
            length = min(int(1.6 * RATE), n - start)
            if length <= 0:
                continue
            tone = bell(hz(chord[index] + 24), length) * 0.05
            pan = 0.25 + 0.25 * j
            left[start : start + length] += tone * (1 - pan)
            right[start : start + length] += tone * pan
        time += BAR
    return left, right


def kick(length):
    t = np.arange(length) / RATE
    sweep = 46 + 80 * np.exp(-t * 26)
    phase = 2 * np.pi * np.cumsum(sweep) / RATE
    return np.sin(phase) * np.exp(-t * 8) * 0.5


def noise(length, seed, decay, gain):
    rng = np.random.default_rng(seed)
    hiss = rng.standard_normal(length)
    hiss -= np.convolve(hiss, np.ones(6) / 6, mode="same")  # keep only the highs
    t = np.arange(length) / RATE
    return hiss * np.exp(-t * decay) * gain


def drums(duration):
    n = int(duration * RATE)
    out = np.zeros(n)
    beat = 0
    time = BEAT_IN
    while time < min(duration, BEAT_OUT):
        start = int(time * RATE)
        if beat % 4 == 0 or beat % 8 == 6:
            length = min(int(0.35 * RATE), n - start)
            out[start : start + length] += kick(length)
        if beat % 4 == 2:
            length = min(int(0.2 * RATE), n - start)
            out[start : start + length] += noise(length, 1000 + beat, 22, 0.09)  # a brushed snare
        s = int((time + BEAT / 2) * RATE)
        length = min(int(0.06 * RATE), n - s)
        if length > 0:
            out[s : s + length] += noise(length, beat, 80, 0.04)
        beat += 1
        time += BEAT
    return out


def master(left, right, duration):
    n = int(duration * RATE)
    t = np.arange(n) / RATE
    fade = np.clip(t / 1.0, 0, 1) * np.clip((duration - t) / 3.0, 0, 1)
    stereo = np.stack((left[:n] * fade, right[:n] * fade))
    stereo = np.tanh(stereo * 1.4) / 1.4
    rms = math.sqrt(float(np.mean(stereo**2)))
    stereo *= 10 ** (-20 / 20) / max(rms, 1e-9)
    peak = float(np.max(np.abs(stereo)))
    if peak > 10 ** (-1 / 20):
        stereo *= 10 ** (-1 / 20) / peak
    return stereo


def write(destination, stereo):
    destination.parent.mkdir(parents=True, exist_ok=True)
    data = (np.clip(stereo.T, -1, 1) * 32767).astype("<i2")
    with wave.open(str(destination), "wb") as f:
        f.setnchannels(2)
        f.setsampwidth(2)
        f.setframerate(RATE)
        f.writeframes(data.tobytes())


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--duration", type=float, default=54)
    args = p.parse_args()
    d = args.duration
    pl, pr = pad(d)
    b = bass(d)
    sl, sr = sequence(d)
    bl, br = bells(d)
    k = drums(d)
    stereo = master(pl + b + sl + bl + k, pr + b + sr + br + k, d)
    write(args.output, stereo)
    print(f"Wrote {args.output}", flush=True)


if __name__ == "__main__":
    main()
