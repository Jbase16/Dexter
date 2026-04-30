#!/usr/bin/env python3
"""Dexter TTS worker — kokoro KPipeline with 24kHz→16kHz resampling.

Threading architecture
──────────────────────
The main thread runs the stdin message loop exclusively. A daemon synthesis
thread pulls text items from `synth_queue`, calls the blocking KPipeline
generator, and writes PCM frames + DONE to stdout.

This separation is required because KPipeline.__call__ is a blocking generator
that can run for several seconds per utterance. If synthesis ran inline in the
message loop, HEALTH_PING messages would accumulate unanswered during synthesis.
The Rust health-check timeout (3 s) would fire and kill the worker mid-sentence.

stdout_lock serialises all send_frame calls across both threads. Without it,
a HEALTH_PONG write and a TTS_AUDIO write could interleave at the byte level,
corrupting the binary length-prefix framing protocol.
"""
import queue
import sys
import threading

import numpy as np
import scipy.signal
from kokoro import KPipeline

from workers.protocol import (
    write_handshake, send_frame, read_frame,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_TEXT_INPUT, MSG_TTS_AUDIO, MSG_TTS_DONE, MSG_SHUTDOWN,
)

KOKORO_VOICE = "am_michael"
KOKORO_RATE  = 24_000   # kokoro native output sample rate
TARGET_RATE  = 16_000   # Dexter pipeline sample rate


def resample_to_16k(audio_f32: np.ndarray) -> bytes:
    """Resample float32 24kHz → int16 16kHz PCM."""
    # resample_poly(x, up, down): up=2, down=3 → ×(2/3) = 16k/24k
    resampled = scipy.signal.resample_poly(audio_f32, 2, 3)
    return (np.clip(resampled, -1.0, 1.0) * 32767).astype(np.int16).tobytes()


def synthesis_worker(
    pipeline:   KPipeline,
    synth_queue: "queue.Queue[str | None]",
    stdout:     object,
    stdout_lock: threading.Lock,
) -> None:
    """Background thread: synthesise queued text items and write PCM to stdout.

    Runs until a `None` sentinel is pulled from the queue (SHUTDOWN signal).
    Calls the blocking KPipeline generator; the main thread remains free to
    handle HEALTH_PING messages while synthesis is in progress.
    """
    while True:
        text = synth_queue.get()
        if text is None:
            break   # SHUTDOWN sentinel — exit cleanly

        # Phase 38 / Codex finding [14]: ALWAYS emit MSG_TTS_DONE at the end
        # of an utterance, even if KPipeline raises mid-synthesis. Pre-Phase-38
        # an exception inside the for-loop would skip the DONE send entirely,
        # leaving the Rust read loop parked at `read_frame()` indefinitely
        # (no per-frame timeout existed). The Rust side now has its own per-
        # frame timeout (Codex [14] Rust half), but emitting DONE on the Python
        # side is strictly better — the worker stays usable for the next
        # utterance instead of being marked dead by the Rust timeout.
        try:
            for _, _, audio in pipeline(text, voice=KOKORO_VOICE, speed=1.0):
                pcm = resample_to_16k(audio)
                with stdout_lock:
                    send_frame(stdout, MSG_TTS_AUDIO, pcm)
        except Exception as e:  # noqa: BLE001 — defensive: any synth error must still emit DONE
            print(
                f"[tts_worker] synthesis raised during utterance: {type(e).__name__}: {e}",
                file=sys.stderr,
            )
            sys.stderr.flush()
        finally:
            # Always — even on KPipeline raise, even on resample raise — tell
            # Rust the utterance is done so its read loop unblocks and the
            # generation completion path can proceed.
            with stdout_lock:
                send_frame(stdout, MSG_TTS_DONE)


def main() -> None:
    # ── Model init ────────────────────────────────────────────────────────────
    #
    # Load model before signalling ready — health checks start immediately after
    # handshake and the 3-second health timeout would fire during a ~3s model load.
    #
    # Silence stdout at the file-descriptor level during init. kokoro prints a
    # "Defaulting repo_id" warning to stdout (fd 1) which would be read by Rust
    # as the handshake line and fail JSON parsing. os.dup2 captures writes from
    # both Python-level print() and C-extension writes that bypass sys.stdout.
    import os
    _saved_stdout_fd = os.dup(1)
    with open(os.devnull, 'wb') as _devnull:
        os.dup2(_devnull.fileno(), 1)
        try:
            pipeline = KPipeline(lang_code='a')
        finally:
            # Flush Python's text and binary stdout buffers while fd 1 still points
            # to /dev/null. Without this, any buffered print() output from KPipeline
            # (e.g. "WARNING: Defaulting repo_id...") drains to the restored fd 1
            # immediately before write_handshake, corrupting the protocol stream.
            sys.stdout.flush()
            sys.stdout.buffer.flush()
            os.dup2(_saved_stdout_fd, 1)
            os.close(_saved_stdout_fd)

    write_handshake(sys.stdout.buffer, "tts")
    stdin       = sys.stdin.buffer
    stdout      = sys.stdout.buffer
    stdout_lock = threading.Lock()
    synth_queue: queue.Queue[str | None] = queue.Queue()

    # ── Synthesis thread ──────────────────────────────────────────────────────
    synth_thread = threading.Thread(
        target=synthesis_worker,
        args=(pipeline, synth_queue, stdout, stdout_lock),
        daemon=True,
        name="dexter-tts-synth",
    )
    synth_thread.start()

    # ── Main message loop ─────────────────────────────────────────────────────
    #
    # Reads from stdin exclusively. Health pings are answered immediately here
    # regardless of whether synthesis is in progress on the background thread.
    try:
        while True:
            msg_type, payload = read_frame(stdin)
            if msg_type is None:
                break

            if msg_type == MSG_HEALTH_PING:
                with stdout_lock:
                    send_frame(stdout, MSG_HEALTH_PONG)

            elif msg_type == MSG_TEXT_INPUT:
                text = payload.decode('utf-8')
                synth_queue.put(text)

            elif msg_type == MSG_SHUTDOWN:
                break
    finally:
        synth_queue.put(None)   # signal synthesis thread to stop
        synth_thread.join(timeout=10)


if __name__ == "__main__":
    main()
