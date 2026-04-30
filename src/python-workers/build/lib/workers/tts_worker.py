#!/usr/bin/env python3
"""Dexter TTS worker — kokoro KPipeline with 24kHz→16kHz resampling."""
import sys
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


def main() -> None:
    write_handshake(sys.stdout.buffer, "tts")
    pipeline = KPipeline(lang_code='a')  # 'a' = American English
    stdin, stdout = sys.stdin.buffer, sys.stdout.buffer

    while True:
        msg_type, payload = read_frame(stdin)
        if msg_type is None:
            break

        if msg_type == MSG_HEALTH_PING:
            send_frame(stdout, MSG_HEALTH_PONG)

        elif msg_type == MSG_TEXT_INPUT:
            text = payload.decode('utf-8')
            for _, _, audio in pipeline(text, voice=KOKORO_VOICE, speed=1.0):
                pcm = resample_to_16k(audio)
                send_frame(stdout, MSG_TTS_AUDIO, pcm)
            send_frame(stdout, MSG_TTS_DONE)

        elif msg_type == MSG_SHUTDOWN:
            break


if __name__ == "__main__":
    main()
