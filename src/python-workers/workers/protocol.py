"""
Dexter voice worker IPC protocol.

Binary frame format: [1 byte msg_type][4 bytes payload_len LE u32][payload_len bytes]
Handshake: worker writes one newline-terminated JSON line on stdout before binary mode.

Constants must stay in sync with Rust src/rust-core/src/voice/protocol.rs.
"""
import json
import struct
from typing import IO

PROTOCOL_VERSION = 1
HEADER_SIZE      = 5   # 1 byte type + 4-byte LE u32 length

MSG_HEALTH_PING = 0x01
MSG_HEALTH_PONG = 0x02
MSG_AUDIO_CHUNK = 0x03
MSG_AUDIO_END   = 0x04
MSG_TRANSCRIPT  = 0x05
MSG_TEXT_INPUT  = 0x06
MSG_TTS_AUDIO   = 0x07
MSG_TTS_DONE    = 0x08
MSG_SHUTDOWN    = 0x09
MSG_ERROR       = 0x0A

# Browser worker (Phase 14) — must stay in sync with Rust voice/protocol.rs
MSG_BROWSER_NAVIGATE   = 0x0B  # payload: JSON {"url": "..."}
MSG_BROWSER_CLICK      = 0x0C  # payload: JSON {"selector": "..."}
MSG_BROWSER_TYPE       = 0x0D  # payload: JSON {"selector": "...", "text": "..."}
MSG_BROWSER_EXTRACT    = 0x0E  # payload: JSON {"selector": null | "..."}
MSG_BROWSER_SCREENSHOT = 0x0F  # payload: empty — worker saves path, returns in result
MSG_BROWSER_RESULT     = 0x10  # payload: JSON {"success": bool, "output": "...", "error": "..."}

# STT utterance boundary (Phase 23) — sent after all TRANSCRIPT frames for one utterance.
# Allows the STT worker to remain alive across multiple utterances without closing stdout.
# Must stay in sync with Rust voice/protocol.rs msg::TRANSCRIPT_DONE.
MSG_TRANSCRIPT_DONE    = 0x11  # payload: empty


def send_frame(f: IO[bytes], msg_type: int, payload: bytes = b'') -> None:
    """Write one binary frame to a file-like object."""
    header = struct.pack('<BI', msg_type, len(payload))
    f.write(header + payload)
    f.flush()


def read_frame(f: IO[bytes]) -> tuple[int | None, bytes | None]:
    """Read one binary frame. Returns (None, None) on EOF or truncated frame."""
    header = f.read(HEADER_SIZE)
    if len(header) < HEADER_SIZE:
        return None, None
    msg_type, length = struct.unpack('<BI', header)
    payload = f.read(length) if length > 0 else b''
    if length > 0 and len(payload) < length:
        return None, None
    return msg_type, payload


def write_handshake(f: IO[bytes], worker_type: str) -> None:
    """Write the JSON handshake line to stdout before entering binary mode."""
    line = json.dumps({"protocol_version": PROTOCOL_VERSION, "worker_type": worker_type})
    f.write((line + '\n').encode())
    f.flush()
