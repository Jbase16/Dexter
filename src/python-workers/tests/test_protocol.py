"""Tests for workers/protocol.py — no subprocess, all I/O via BytesIO."""
import io
import json
import struct

import pytest

from workers.protocol import (
    HEADER_SIZE, PROTOCOL_VERSION,
    MSG_HEALTH_PING, MSG_HEALTH_PONG,
    MSG_AUDIO_CHUNK, MSG_AUDIO_END, MSG_TRANSCRIPT,
    MSG_TEXT_INPUT, MSG_TTS_AUDIO, MSG_TTS_DONE,
    MSG_SHUTDOWN, MSG_ERROR,
    send_frame, read_frame, write_handshake,
)


def test_send_and_read_frame_roundtrip():
    """Write a frame to a BytesIO buffer, seek back, read it — payload must be intact."""
    buf = io.BytesIO()
    payload = b"hello world"
    send_frame(buf, MSG_AUDIO_CHUNK, payload)
    buf.seek(0)
    msg_type, data = read_frame(buf)
    assert msg_type == MSG_AUDIO_CHUNK
    assert data == payload


def test_send_frame_zero_payload():
    """msg_type with no payload (e.g. HEALTH_PING) — read back returns empty bytes."""
    buf = io.BytesIO()
    send_frame(buf, MSG_HEALTH_PING)
    buf.seek(0)
    msg_type, data = read_frame(buf)
    assert msg_type == MSG_HEALTH_PING
    assert data == b''


def test_read_frame_eof_returns_none_none():
    """Empty stream returns (None, None)."""
    buf = io.BytesIO(b'')
    msg_type, data = read_frame(buf)
    assert msg_type is None
    assert data is None


def test_read_frame_truncated_header():
    """Partial header (3 bytes) returns (None, None)."""
    buf = io.BytesIO(b'\x01\x02\x03')   # shorter than HEADER_SIZE (5)
    msg_type, data = read_frame(buf)
    assert msg_type is None
    assert data is None


def test_write_handshake_valid_json():
    """Handshake output is valid JSON with protocol_version and worker_type."""
    buf = io.BytesIO()
    write_handshake(buf, "stt")
    buf.seek(0)
    line = buf.read().decode()
    assert line.endswith('\n')
    parsed = json.loads(line.strip())
    assert parsed["protocol_version"] == PROTOCOL_VERSION
    assert parsed["worker_type"] == "stt"


def test_constants_unique_and_in_range():
    """All 10 MSG_* values are distinct and in range 0x01–0x0A."""
    all_constants = [
        MSG_HEALTH_PING, MSG_HEALTH_PONG,
        MSG_AUDIO_CHUNK, MSG_AUDIO_END, MSG_TRANSCRIPT,
        MSG_TEXT_INPUT, MSG_TTS_AUDIO, MSG_TTS_DONE,
        MSG_SHUTDOWN, MSG_ERROR,
    ]
    assert len(set(all_constants)) == 10, "All MSG_* values must be unique"
    for c in all_constants:
        assert 0x01 <= c <= 0x0A, f"MSG constant {c:#04x} out of range 0x01–0x0A"
