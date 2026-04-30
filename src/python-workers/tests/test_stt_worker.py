"""Tests for workers/stt_worker.py — no live model, all I/O via BytesIO."""
import io
import json
import struct
from unittest.mock import patch, MagicMock

import pytest

from workers.protocol import (
    send_frame, read_frame,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_AUDIO_CHUNK, MSG_AUDIO_END, MSG_TRANSCRIPT, MSG_TRANSCRIPT_DONE, MSG_SHUTDOWN,
)


def _build_input(*frames: tuple[int, bytes]) -> io.BytesIO:
    """Build a BytesIO stream containing the given (msg_type, payload) frames."""
    buf = io.BytesIO()
    for msg_type, payload in frames:
        send_frame(buf, msg_type, payload)
    buf.seek(0)
    return buf


def _mock_info(language: str = "en", prob: float = 0.99) -> MagicMock:
    """Build a mock TranscriptionInfo with the attributes our diagnostic code reads."""
    info = MagicMock()
    info.language = language
    info.language_probability = prob
    return info


def test_health_ping_returns_pong():
    """HEALTH_PING → HEALTH_PONG, then SHUTDOWN exits."""
    stdin  = _build_input((MSG_HEALTH_PING, b''), (MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    mock_model = MagicMock()
    mock_model.transcribe.return_value = ([], _mock_info())

    with patch('workers.stt_worker.WhisperModel', return_value=mock_model), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import stt_worker
        stt_worker.main()

    # Skip the handshake JSON line, then read first binary frame.
    stdout.seek(0)
    handshake_line = stdout.readline()   # newline-terminated JSON
    msg_type, data = read_frame(stdout)
    assert msg_type == MSG_HEALTH_PONG


def test_audio_end_empty_buffer_sends_transcript_done_no_transcript():
    """AUDIO_END with no preceding AUDIO_CHUNK → model not called, only TRANSCRIPT_DONE sent.

    Phase 23: even for empty buffers the worker sends MSG_TRANSCRIPT_DONE so Rust
    knows the utterance processing is complete and can release the mutex.
    """
    stdin  = _build_input((MSG_AUDIO_END, b''), (MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    mock_model = MagicMock()
    mock_model.transcribe.return_value = ([], _mock_info())

    with patch('workers.stt_worker.WhisperModel', return_value=mock_model), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import stt_worker
        stt_worker.main()

    mock_model.transcribe.assert_not_called()

    # Expect exactly TRANSCRIPT_DONE after the handshake — no TRANSCRIPT frames.
    stdout.seek(0)
    stdout.readline()   # handshake
    msg_type, _ = read_frame(stdout)
    assert msg_type == MSG_TRANSCRIPT_DONE, \
        "Empty-buffer AUDIO_END must send TRANSCRIPT_DONE (no TRANSCRIPT before it)"
    # Nothing after TRANSCRIPT_DONE
    msg_type2, _ = read_frame(stdout)
    assert msg_type2 is None, "No further frames expected after TRANSCRIPT_DONE"


def test_audio_end_calls_model_transcribe():
    """AUDIO_CHUNK + AUDIO_END → WhisperModel.transcribe called with float32 array."""
    import numpy as np
    pcm_bytes = (np.zeros(160, dtype=np.int16)).tobytes()   # 160 silent samples

    stdin  = _build_input(
        (MSG_AUDIO_CHUNK, pcm_bytes),
        (MSG_AUDIO_END, b''),
        (MSG_SHUTDOWN, b''),
    )
    stdout = io.BytesIO()

    mock_seg = MagicMock()
    mock_seg.text  = "hello"
    mock_seg.start = 0.0    # :.2f format requires float; MagicMock doesn't support it
    mock_seg.end   = 0.5
    mock_model = MagicMock()
    # Return a proper mock info object — our diagnostic code reads info.language.
    mock_model.transcribe.return_value = ([mock_seg], _mock_info())

    with patch('workers.stt_worker.WhisperModel', return_value=mock_model), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import stt_worker
        stt_worker.main()

    mock_model.transcribe.assert_called_once()
    # First positional arg should be a float32 ndarray.
    call_args = mock_model.transcribe.call_args
    audio_arg = call_args[0][0]
    assert audio_arg.dtype == np.float32


def test_audio_end_sends_transcript_then_done():
    """AUDIO_CHUNK + AUDIO_END with one segment → TRANSCRIPT then TRANSCRIPT_DONE."""
    import numpy as np
    pcm_bytes = (np.ones(160, dtype=np.int16) * 1000).tobytes()

    stdin  = _build_input(
        (MSG_AUDIO_CHUNK, pcm_bytes),
        (MSG_AUDIO_END, b''),
        (MSG_SHUTDOWN, b''),
    )
    stdout = io.BytesIO()

    mock_seg = MagicMock()
    mock_seg.text = "hello there"
    mock_seg.start = 0.0
    mock_seg.end   = 1.0
    mock_model = MagicMock()
    mock_model.transcribe.return_value = ([mock_seg], _mock_info())

    with patch('workers.stt_worker.WhisperModel', return_value=mock_model), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import stt_worker
        stt_worker.main()

    stdout.seek(0)
    stdout.readline()   # handshake

    # First frame: TRANSCRIPT with the segment text
    msg_type, payload = read_frame(stdout)
    assert msg_type == MSG_TRANSCRIPT
    parsed = json.loads(payload)
    assert parsed["text"] == "hello there"
    assert parsed["is_final"] is True

    # Second frame: TRANSCRIPT_DONE (utterance boundary sentinel)
    msg_type2, _ = read_frame(stdout)
    assert msg_type2 == MSG_TRANSCRIPT_DONE, \
        "TRANSCRIPT_DONE must follow all TRANSCRIPT frames for one utterance"


def test_shutdown_exits_main_loop():
    """SHUTDOWN → main() returns without calling transcribe."""
    stdin  = _build_input((MSG_SHUTDOWN, b''))
    stdout = io.BytesIO()

    mock_model = MagicMock()

    with patch('workers.stt_worker.WhisperModel', return_value=mock_model), \
         patch('sys.stdin',  new=MagicMock(buffer=stdin)), \
         patch('sys.stdout', new=MagicMock(buffer=stdout)):
        from workers import stt_worker
        stt_worker.main()   # must return (not hang)

    mock_model.transcribe.assert_not_called()
