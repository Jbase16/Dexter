pub mod server;
pub use server::serve;

/// Re-export the generated proto types at the `ipc` module boundary.
///
/// Consumers (orchestrator, integration tests) access proto types via `crate::ipc::proto`
/// rather than the internal `crate::ipc::server::proto` path — keeps the server module's
/// internal structure opaque while making the proto contract publicly accessible.
pub use server::proto;
