/// Voice Worker Bridge — Phase 10.
///
/// ## Module structure
/// - `protocol`      — IPC binary framing, handshake, message type constants
/// - `sentence`      — SentenceSplitter for token-stream sentence detection
/// - `worker_client` — WorkerClient: subprocess spawn + frame I/O
/// - `coordinator`   — VoiceCoordinator: TTS lifecycle, health checks, restart
pub mod coordinator;
pub mod protocol;
pub mod sentence;
pub mod worker_client;

pub use coordinator::VoiceCoordinator;
pub use protocol::WorkerType;
#[allow(unused_imports)] // Phase 13 callers use WorkerClient directly
pub use worker_client::{WorkerClient, WorkerError};
