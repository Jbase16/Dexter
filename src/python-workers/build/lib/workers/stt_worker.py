#!/usr/bin/env python3
"""Dexter STT worker — faster-whisper base.en over IPC binary protocol."""
import sys
import json
import numpy as np
from faster_whisper import WhisperModel
from workers.protocol import (
    write_handshake, send_frame, read_frame,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_AUDIO_CHUNK, MSG_AUDIO_END, MSG_TRANSCRIPT, MSG_SHUTDOWN,
)


def main() -> None:
    write_handshake(sys.stdout.buffer, "stt")
    model = WhisperModel("base.en", device="cpu", compute_type="int8")
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
                # Convert 16-bit signed PCM → float32 [-1, 1] expected by faster-whisper.
                audio_np = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
                segments, _ = model.transcribe(audio_np, beam_size=5, language="en")
                for seg in segments:
                    text = seg.text.strip()
                    if text:
                        data = json.dumps(
                            {"text": text, "is_final": True, "sequence": sequence}
                        ).encode()
                        send_frame(stdout, MSG_TRANSCRIPT, data)
                        sequence += 1

        elif msg_type == MSG_SHUTDOWN:
            break


if __name__ == "__main__":
    main()
