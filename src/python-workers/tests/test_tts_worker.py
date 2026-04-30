"""Tests for workers/tts_worker.py — no live model, all I/O via BytesIO."""
import io
from unittest.mock import patch, MagicMock, call

import numpy as np
import pytest

from workers.protocol import (
    send_frame, read_frame,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_TEXT_INPUT, MSG_TTS_AUDIO, MSG_TTS_DONE, MSG_SHUTDOWN,
)


def _build_input(*frames: tuple[int, bytes]) -> io.BytesIO:
    buf = io.BytesIO()
    for msg_type, payload in frames:
        send_frame(buf, msg_type, payload)
    buf.seek(0)
    return buf


def test_health_ping_returns_pong():
    """HEALTH_PING → HEALTH_PONG, then SHUTDOWN exits."""
    stdin  = _build_input((MSG_HEALTH_PING, b''), (MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    mock_pipeline = MagicMock(return_value=[])

    with patch('workers.tts_worker.KPipeline', return_value=mock_pipeline), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import tts_worker
        tts_worker.main()

    stdout.seek(0)
    stdout.readline()   # handshake JSON line
    msg_type, _ = read_frame(stdout)
    assert msg_type == MSG_HEALTH_PONG


def test_text_input_calls_pipeline():
    """TEXT_INPUT → KPipeline called with correct text and voice."""
    text = b"Hello, world."
    stdin  = _build_input((MSG_TEXT_INPUT, text), (MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    # Pipeline returns one audio chunk.
    fake_audio = np.zeros(240, dtype=np.float32)
    mock_pipeline = MagicMock(return_value=[('', '', fake_audio)])

    with patch('workers.tts_worker.KPipeline', return_value=mock_pipeline), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import tts_worker
        tts_worker.main()

    mock_pipeline.assert_called_once_with(
        text.decode('utf-8'),
        voice="af_heart",
        speed=1.0,
    )


def test_tts_done_follows_audio_frames():
    """All TTS_AUDIO frames precede a single TTS_DONE."""
    text = b"Test sentence."
    stdin  = _build_input((MSG_TEXT_INPUT, text), (MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    # Return two audio chunks.
    chunk = np.zeros(240, dtype=np.float32)
    mock_pipeline = MagicMock(return_value=[('', '', chunk), ('', '', chunk)])

    with patch('workers.tts_worker.KPipeline', return_value=mock_pipeline), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import tts_worker
        tts_worker.main()

    stdout.seek(0)
    stdout.readline()   # handshake

    types: list[int] = []
    while True:
        mt, _ = read_frame(stdout)
        if mt is None:
            break
        types.append(mt)

    # Expect: TTS_AUDIO, TTS_AUDIO, TTS_DONE
    assert types.count(MSG_TTS_AUDIO) == 2
    assert types[-1] == MSG_TTS_DONE, "TTS_DONE must be the final frame"
    assert types.index(MSG_TTS_DONE) == len(types) - 1


def test_resample_to_16k_output_dtype():
    """resample_to_16k returns bytes; length ≈ input * (2/3); int16 compatible."""
    from workers.tts_worker import resample_to_16k
    # 240 float32 samples at 24kHz → 160 int16 samples at 16kHz
    audio = np.ones(240, dtype=np.float32) * 0.5
    result = resample_to_16k(audio)
    assert isinstance(result, bytes)
    # Each int16 is 2 bytes; expected ~160 samples = 320 bytes.
    n_samples = len(result) // 2
    assert 140 <= n_samples <= 180, f"Expected ~160 samples, got {n_samples}"
    # Reinterpret as int16 — must not raise.
    arr = np.frombuffer(result, dtype=np.int16)
    assert len(arr) == n_samples
