#!/usr/bin/env python3
"""Dexter STT worker — faster-whisper base.en over IPC binary protocol.

Phase 23: persistent worker — model is loaded once and reused across utterances.
Lifecycle: handshake (after model load) → loop {receive chunks → transcribe → respond}.

Protocol per utterance:
  Inbound:  N × MSG_AUDIO_CHUNK, then MSG_AUDIO_END
  Outbound: 0–N × MSG_TRANSCRIPT (one per non-empty segment), then MSG_TRANSCRIPT_DONE

MSG_TRANSCRIPT_DONE signals end-of-utterance without closing the worker.  Rust reads
frames until it receives TRANSCRIPT_DONE, then releases the mutex, leaving the worker
alive and ready for the next utterance.
"""
import sys
import json
import numpy as np
from faster_whisper import WhisperModel
from workers.protocol import (
    write_handshake, send_frame, read_frame,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_AUDIO_CHUNK, MSG_AUDIO_END, MSG_TRANSCRIPT, MSG_TRANSCRIPT_DONE, MSG_SHUTDOWN,
)


def main() -> None:
    # Load model BEFORE writing handshake — same pattern as tts_worker.py.
    # Rust considers the worker "ready" only after it receives the handshake.
    # Loading first ensures the read loop is running before any audio arrives,
    # avoiding the pipe-buffer overflow that truncates long utterances.
    print("[stt_worker] Loading WhisperModel base.en…", file=sys.stderr)
    sys.stderr.flush()
    model = WhisperModel("base.en", device="cpu", compute_type="int8")
    print("[stt_worker] Model loaded — writing handshake", file=sys.stderr)
    sys.stderr.flush()

    write_handshake(sys.stdout.buffer, "stt")

    stdin, stdout = sys.stdin.buffer, sys.stdout.buffer
    audio_buffer: list[bytes] = []
    sequence = 0

    while True:
        msg_type, payload = read_frame(stdin)
        if msg_type is None:
            break

        if msg_type == MSG_HEALTH_PING:
            send_frame(stdout, MSG_HEALTH_PONG)

        elif msg_type == MSG_AUDIO_CHUNK:
            audio_buffer.append(payload)

        elif msg_type == MSG_AUDIO_END:
            if audio_buffer:
                raw = b''.join(audio_buffer)
                audio_buffer.clear()
                duration_ms = len(raw) // 32  # 16 kHz * 2 bytes/sample → 32 bytes/ms
                print(f"[stt_worker] Transcribing {len(raw)} bytes ({duration_ms} ms audio)",
                      file=sys.stderr)
                sys.stderr.flush()

                # Convert 16-bit signed PCM → float32 [-1, 1] expected by faster-whisper.
                audio_np = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
                segments, info = model.transcribe(audio_np, beam_size=5, language="en")

                # Materialise the lazy generator before iterating — needed to log count.
                segment_list = list(segments)
                print(f"[stt_worker] Got {len(segment_list)} segment(s) "
                      f"(lang={info.language}, prob={info.language_probability:.2f})",
                      file=sys.stderr)
                sys.stderr.flush()

                # Phase 38 / Codex finding [12]: aggregate ALL non-empty segments
                # into ONE transcript per AUDIO_END. Pre-Phase-38 each segment
                # was sent as a separate MSG_TRANSCRIPT with `is_final: True` and
                # an incrementing sequence — the Rust fast path treated each as
                # a separate user turn. Long utterances with internal pauses
                # ("text my mom… and tell her I'll be late") got split into
                # multiple turns, with the model responding to each fragment
                # independently. Aggregation preserves the operator's intent:
                # one button press → one utterance → one transcript → one turn.
                segment_texts: list[str] = []
                for seg in segment_list:
                    text = seg.text.strip()
                    print(f"[stt_worker]   [{seg.start:.2f}s–{seg.end:.2f}s]: {repr(text)}",
                          file=sys.stderr)
                    if text:
                        segment_texts.append(text)

                if segment_texts:
                    transcript = " ".join(segment_texts)
                    data = json.dumps(
                        {"text": transcript, "is_final": True, "sequence": sequence}
                    ).encode()
                    send_frame(stdout, MSG_TRANSCRIPT, data)
                    sequence += 1
                else:
                    print("[stt_worker] No non-empty segments — transcript empty",
                          file=sys.stderr)
                sys.stderr.flush()
            else:
                audio_buffer.clear()
                print("[stt_worker] AUDIO_END with empty buffer — skipping", file=sys.stderr)
                sys.stderr.flush()

            # Always send TRANSCRIPT_DONE so Rust knows the utterance is complete
            # and can release the mutex without killing the worker.
            send_frame(stdout, MSG_TRANSCRIPT_DONE)

        elif msg_type == MSG_SHUTDOWN:
            break


if __name__ == "__main__":
    main()
