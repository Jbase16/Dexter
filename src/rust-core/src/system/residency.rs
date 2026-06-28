//! Cross-process model-weight residency pinning.
//!
//! ## What this kills
//!
//! `PRIMARY_KEEPALIVE_PING_INTERVAL_SECS` (constants.rs) is a 180→90→60→45→30 s
//! death-march against macOS page reclamation. The keepalive ping is *reactive*:
//! it re-faults PRIMARY's weight pages by running a 1-token generation, which
//! means the ping itself eats the cold-load it was meant to prevent (the
//! `load_ms ≈ 21000` lines in the daemon log). It loses the race under sustained
//! pressure by construction — see the "FIVE consecutive cold-loads" note in
//! `constants.rs`.
//!
//! ## The mechanism (why pinning from *outside* Ollama works)
//!
//! `mlock(2)` only operates on the caller's own address space — so the obvious
//! idea, "patch Ollama to mlock its weights," requires patching Ollama. We don't.
//!
//! On XNU, a file's pages live in a single vnode-pager-backed memory object in
//! the Unified Buffer Cache, **shared by inode** across every process that maps
//! the file. When Ollama maps the GGUF blob (which requires forcing
//! `use_mmap: true` — see the "Hard dependency" note below; Ollama does NOT mmap
//! by default on this config), llama.cpp on Apple Silicon wraps those mmap'd
//! pages *zero-copy* into a Metal buffer via `newBufferWithBytesNoCopy:` —
//! unified memory means the GPU reads the same physical frames as the file page
//! cache (there is no separate VRAM copy). Those frames are pageable and
//! file-backed, which is what makes a cross-process pin possible.
//!
//! A *separate* process (the Dexter daemon) that `mmap`s the same blob inode and
//! calls `mlock` wires those shared page-cache frames in physical memory. Because
//! Ollama's mapping resolves to the same UBC object, its view — and the Metal
//! buffer aliasing it — is now non-reclaimable. One process's wire pins the pages
//! for the other. No Ollama patch. No GPU duty cycle. The pin is a one-time cost,
//! not a 30 s heartbeat.
//!
//! ## Tier state machine (why this fits Dexter specifically)
//!
//! 36 GB unified memory cannot hold PRIMARY (~18 GB) and HEAVY (~19 GB) at once.
//! So residency is tied to routing, not pinned forever:
//!   - PRIMARY warm  → `pin_primary` (wire 18 GB; keepalive ping retires)
//!   - HEAVY routed  → `unpin_primary` before Ollama unloads it (pages become
//!                     reclaimable so HEAVY can load), optionally `pin_heavy`
//!   - HEAVY done    → `unpin_heavy`, rewarm PRIMARY, `pin_primary` again
//!
//! `unpin_primary` on the HEAVY path is **load-bearing**: without it, PRIMARY's
//! 18 GB stays wired and HEAVY's load OOMs.
//!
//! ## Hard dependency: `use_mmap: true`
//!
//! This whole scheme is INERT unless Ollama memory-maps the blob. Empirically
//! (verified live via `ps`/`vmmap` on the running `llama-server`), Ollama
//! defaults to `--no-mmap` on this Apple-Silicon full-GPU config — weights load
//! into anonymous memory private to the runner, which a cross-process `mlock`
//! cannot touch. The inference engine therefore sends `use_mmap: true` on every
//! request (see `OllamaOptions.use_mmap` in `inference/engine.rs`), which makes
//! the runner map the blob as a SHARED (`SM=SHM`) mapping whose UBC pages this
//! module wires. If that flag is ever dropped, the pin silently does nothing —
//! `engine::tests::generation_options_request_mmap` guards against it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

/// macOS `mincore` residency bit. libc does not export `MINCORE_INCORE` on this
/// target, so we define it. Bit 0 set => the page is resident in physical memory.
const MINCORE_INCORE: u8 = 0x1;

// ── PinnedRegion ────────────────────────────────────────────────────────────

/// One `mmap`'d + `mlock`'d file region. `Drop` munlocks and munmaps.
struct PinnedRegion {
    addr: *mut libc::c_void,
    len: usize,
    path: PathBuf,
}

// SAFETY: `addr` is a read-only mapping owned exclusively by this region for its
// entire lifetime; the pages are never written or aliased mutably from Rust.
// Moving the region between threads only transfers ownership of that handle, and
// the only operations performed on it (mincore, munlock, munmap) are valid from
// any thread. The region is therefore Send + Sync.
unsafe impl Send for PinnedRegion {}
unsafe impl Sync for PinnedRegion {}

impl PinnedRegion {
    /// `mmap` the file read-only (MAP_SHARED, so we map the UBC pages directly,
    /// not a private copy) and `mlock` the whole range. `mlock` both faults the
    /// pages in and wires them; on return every page is resident and non-
    /// reclaimable until `Drop`.
    fn map_and_lock(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to pin a zero-length blob",
            ));
        }

        // SAFETY: standard read-only mmap of an open fd. null hint lets the
        // kernel choose the address; offset 0, full length.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // The mapping stays valid after the fd is closed (POSIX). `file` drops
        // here, closing the fd; we keep only `addr`/`len`.
        drop(file);

        // SAFETY: addr/len describe the mapping we just created.
        let rc = unsafe { libc::mlock(addr, len) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: unmap the region we failed to lock so we don't leak it.
            unsafe {
                libc::munmap(addr, len);
            }
            return Err(err);
        }

        Ok(Self {
            addr,
            len,
            path: path.to_path_buf(),
        })
    }

    /// Fraction of this mapping's pages currently resident in physical memory,
    /// via `mincore`. Returns a value in `[0.0, 1.0]`. For a freshly `mlock`'d
    /// region this is `1.0`; the proof harness also calls it on an *observer*
    /// mapping to show cross-process residency.
    fn resident_fraction(&self) -> f64 {
        let page = page_size();
        let pages = self.len.div_ceil(page);
        let mut vec = vec![0u8; pages];
        // SAFETY: addr/len are our mapping; vec has one byte per page.
        let rc = unsafe {
            libc::mincore(
                self.addr,
                self.len,
                vec.as_mut_ptr() as *mut std::os::raw::c_char,
            )
        };
        if rc != 0 {
            return f64::NAN;
        }
        let resident = vec.iter().filter(|b| *b & MINCORE_INCORE != 0).count();
        resident as f64 / pages as f64
    }
}

impl Drop for PinnedRegion {
    fn drop(&mut self) {
        // SAFETY: addr/len describe a mapping we own and locked. munlock before
        // munmap so the wire count is released even if munmap is deferred.
        unsafe {
            libc::munlock(self.addr, self.len);
            libc::munmap(self.addr, self.len);
        }
        info!(path = %self.path.display(), bytes = self.len, "Residency: unpinned (munlock+munmap)");
    }
}

// ── ResidencyManager ────────────────────────────────────────────────────────

struct Inner {
    primary: Option<PinnedRegion>,
    heavy: Option<PinnedRegion>,
}

/// Daemon-lifetime owner of pinned model regions. Cloneable (Arc-internally) so
/// it can live in `SharedDaemonState` and be reached from any session.
#[derive(Clone)]
pub struct ResidencyManager {
    models_dir: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl ResidencyManager {
    pub fn new(models_dir: PathBuf) -> Self {
        raise_memlock_limit();
        Self {
            models_dir,
            inner: Arc::new(Mutex::new(Inner {
                primary: None,
                heavy: None,
            })),
        }
    }

    /// Resolve the models directory from `OLLAMA_MODELS`, falling back to the
    /// default `~/.ollama/models`. Matches how the Ollama daemon itself resolves
    /// its store, so we pin exactly the blob Ollama maps.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("OLLAMA_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".ollama")
                    .join("models")
            });
        Self::new(dir)
    }

    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Resolve an Ollama model tag (`gemma4:26b`, `mxbai-embed-large`) to the
    /// path of its GGUF weight blob, by reading the on-disk manifest and finding
    /// the `application/vnd.ollama.image.model` layer.
    ///
    /// Returns `None` if the manifest is absent/malformed, the blob is missing,
    /// or the model uses Ollama's tensor-shard/MLX layout. The cross-process
    /// residency mechanism is intentionally GGUF-only: MLX manifests have many
    /// `application/vnd.ollama.image.tensor` layers instead of one mmap'd GGUF
    /// blob, so there is no single blob for this pinner to wire resident.
    pub fn resolve_model_blob(&self, model_tag: &str) -> Option<PathBuf> {
        match self.resolve_model_blob_result(model_tag) {
            ModelBlobResolution::GgufBlob(path) => Some(path),
            ModelBlobResolution::Missing | ModelBlobResolution::TensorShards => None,
        }
    }

    fn resolve_model_blob_result(&self, model_tag: &str) -> ModelBlobResolution {
        let (name, tag) = model_tag.split_once(':').unwrap_or((model_tag, "latest"));
        let manifest = self
            .models_dir
            .join("manifests/registry.ollama.ai/library")
            .join(name)
            .join(tag);
        let bytes = match std::fs::read(&manifest) {
            Ok(bytes) => bytes,
            Err(_) => return ModelBlobResolution::Missing,
        };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return ModelBlobResolution::Missing,
        };
        let Some(layers) = v["layers"].as_array() else {
            return ModelBlobResolution::Missing;
        };
        let digest = match layers
            .iter()
            .find(|l| l["mediaType"].as_str() == Some("application/vnd.ollama.image.model"))
            .and_then(|l| l.get("digest"))
            .and_then(|d| d.as_str())
        {
            Some(digest) => digest,
            None => {
                let has_tensor_layers = layers.iter().any(|l| {
                    l["mediaType"].as_str() == Some("application/vnd.ollama.image.tensor")
                });
                return if has_tensor_layers {
                    ModelBlobResolution::TensorShards
                } else {
                    ModelBlobResolution::Missing
                };
            }
        };
        // "sha256:ab.." → "sha256-ab.." (Ollama's on-disk blob filename form).
        let blob = digest.replacen(':', "-", 1);
        let path = self.models_dir.join("blobs").join(blob);
        if path.exists() {
            ModelBlobResolution::GgufBlob(path)
        } else {
            ModelBlobResolution::Missing
        }
    }

    /// Pin the given model tag's weight blob into the `primary` slot. Returns
    /// `true` on success. Idempotent: re-pinning replaces any existing region.
    pub fn pin_primary(&self, model_tag: &str) -> bool {
        self.pin_slot(model_tag, Slot::Primary)
    }

    pub fn unpin_primary(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.primary = None; // Drop → munlock + munmap
        }
    }

    /// Reserved: HEAVY residency is intentionally NOT wired in the orchestrator.
    /// HEAVY (deepseek-r1:32b) is on-demand and short-lived — it was never the
    /// keepalive-ping victim, and pinning its 19 GB would double-fault against
    /// Ollama's own load. The API is here for symmetry and future use.
    #[allow(dead_code)]
    pub fn pin_heavy(&self, model_tag: &str) -> bool {
        self.pin_slot(model_tag, Slot::Heavy)
    }

    #[allow(dead_code)]
    pub fn unpin_heavy(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.heavy = None;
        }
    }

    pub fn is_primary_pinned(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.primary.is_some())
            .unwrap_or(false)
    }

    fn pin_slot(&self, model_tag: &str, slot: Slot) -> bool {
        let path = match self.resolve_model_blob_result(model_tag) {
            ModelBlobResolution::GgufBlob(path) => path,
            ModelBlobResolution::TensorShards => {
                info!(
                    model = model_tag,
                    models_dir = %self.models_dir.display(),
                    "Residency: model uses tensor-shard/MLX layout; GGUF blob pinning is not applicable, keepalive fallback remains responsible"
                );
                return false;
            }
            ModelBlobResolution::Missing => {
                warn!(
                    model = model_tag,
                    models_dir = %self.models_dir.display(),
                    "Residency: could not resolve GGUF blob for model — leaving keepalive fallback in place"
                );
                return false;
            }
        };
        match PinnedRegion::map_and_lock(&path) {
            Ok(region) => {
                let gb = region.len as f64 / 1_073_741_824.0;
                // Store the region under the lock. If the lock is poisoned we must
                // NOT report success: `region` would drop at the end of this arm
                // (munlock+munmap), leaving nothing pinned. For a subsystem whose
                // contract is "the weights are definitely resident", reporting
                // pinned-when-not-pinned is the single worst failure shape — so we
                // fail closed and let the caller keep the keepalive fallback.
                match self.inner.lock() {
                    Ok(mut g) => {
                        match slot {
                            Slot::Primary => g.primary = Some(region),
                            Slot::Heavy => g.heavy = Some(region),
                        }
                        info!(
                            model = model_tag,
                            slot = slot.as_str(),
                            gb = gb,
                            path = %path.display(),
                            "Residency: weights wired resident"
                        );
                        true
                    }
                    Err(_) => {
                        // region drops here → munlock + munmap. Nothing is pinned.
                        warn!(
                            model = model_tag,
                            slot = slot.as_str(),
                            "Residency: state mutex poisoned — wired region released, reporting NOT pinned so the keepalive fallback stays armed"
                        );
                        false
                    }
                }
            }
            Err(e) => {
                warn!(
                    model = model_tag,
                    error = %e,
                    path = %path.display(),
                    "Residency: mlock failed — falling back to keepalive ping. \
                     If errno is ENOMEM/EAGAIN, raise vm.global_user_wire_limit (SIP must be off)."
                );
                false
            }
        }
    }

    /// Operator-visible residency status for health/doctor surfaces.
    pub fn status(&self) -> ResidencyStatus {
        match self.inner.lock() {
            Ok(g) => ResidencyStatus {
                primary_pinned: g.primary.is_some(),
                primary_wired_bytes: g.primary.as_ref().map(|r| r.len).unwrap_or(0),
                lock_poisoned: false,
            },
            Err(_) => ResidencyStatus {
                primary_pinned: false,
                primary_wired_bytes: 0,
                lock_poisoned: true,
            },
        }
    }
}

#[derive(Clone, Copy)]
enum Slot {
    Primary,
    #[allow(dead_code)] // reserved with pin_heavy — see note there
    Heavy,
}

enum ModelBlobResolution {
    GgufBlob(PathBuf),
    TensorShards,
    Missing,
}

/// Snapshot of residency state for health/doctor reporting.
#[derive(Debug, Clone, Copy)]
pub struct ResidencyStatus {
    pub primary_pinned: bool,
    pub primary_wired_bytes: usize,
    /// True if the residency mutex was poisoned — pin state is unknown and the
    /// keepalive fallback should be considered the source of truth.
    pub lock_poisoned: bool,
}

impl Slot {
    fn as_str(self) -> &'static str {
        match self {
            Slot::Primary => "primary",
            Slot::Heavy => "heavy",
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn raise_memlock_limit() {
    // SAFETY: setrlimit with a valid rlimit pointer. EPERM/failure is non-fatal —
    // on macOS, mlock of moderate sizes typically succeeds without raising this;
    // the wire limit is governed more by `vm.global_user_wire_limit` than RLIMIT.
    unsafe {
        let lim = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        let _ = libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim);
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf is always safe to call.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 {
        p as usize
    } else {
        16384 // Apple Silicon default
    }
}

// ── Proof harness ───────────────────────────────────────────────────────────
//
// Invoked from `main.rs` via `--prove-residency` (and the hidden child role
// `--residency-pin-child`). Demonstrates the load-bearing OS mechanism on REAL
// Ollama blobs, cross-process, with real wired-memory deltas — not a mock.

/// Hidden child role: pin `path` and hold the lock until stdin closes, printing
/// `PINNED <bytes>` once the wire is in place so the parent can measure.
pub fn run_pin_child(path: &str, _hold_secs: u64) {
    use std::io::{BufRead, Write};
    match PinnedRegion::map_and_lock(Path::new(path)) {
        Ok(region) => {
            let resident = region.resident_fraction();
            println!("PINNED {} {:.4}", region.len, resident);
            let _ = std::io::stdout().flush();
            // Hold the lock until the parent closes our stdin (or we're killed).
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            drop(region); // explicit: munlock+munmap before exit
        }
        Err(e) => {
            println!("PIN_FAILED {e}");
            let _ = std::io::stdout().flush();
            std::process::exit(2);
        }
    }
}

/// Parent role: run the full cross-process proof and print a verdict.
pub fn run_proof(model: Option<String>) -> std::io::Result<()> {
    let mgr = ResidencyManager::from_env();
    println!("── Dexter residency proof ──────────────────────────────────────");
    println!("models_dir: {}", mgr.models_dir().display());
    println!("page size : {} bytes", page_size());
    println!();

    // Part B preview: resolve the REAL PRIMARY model blob (the one that suffers
    // the keepalive saga) to show the production resolver targets the right file.
    let primary_tag =
        std::env::var("DEXTER_RESIDENCY_PRIMARY").unwrap_or_else(|_| "gemma4:26b".into());
    match mgr.resolve_model_blob(&primary_tag) {
        Some(p) => {
            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            println!(
                "[resolve] PRIMARY {primary_tag} → {} ({:.1} GB) — this is the blob the daemon would wire",
                p.display(),
                sz as f64 / 1_073_741_824.0
            );
        }
        None => println!("[resolve] PRIMARY {primary_tag} → NOT FOUND in this store"),
    }
    println!();

    // Part A: prove the mechanism cross-process on a SAFE-sized real blob.
    // Default to mxbai-embed-large (~640 MB) so we never wire 18 GB during a test.
    let proof_tag = model.unwrap_or_else(|| "mxbai-embed-large".to_string());
    let Some(blob) = mgr.resolve_model_blob(&proof_tag) else {
        println!("[proof] could not resolve {proof_tag} — is it pulled? (`ollama list`)");
        return Ok(());
    };
    let blob_bytes = std::fs::metadata(&blob)?.len();
    let blob_mb = blob_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "[proof] target blob: {proof_tag} → {} ({:.0} MB)",
        blob.display(),
        blob_mb
    );

    let page = page_size();
    let w0 = wired_bytes();
    println!("[proof] wired baseline (W0)        : {:.0} MB", mb(w0));

    // Spawn a SEPARATE process to do the pinning — this is the whole point:
    // the process that wires the pages is not the process that will use them.
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .arg("--residency-pin-child")
        .arg(&blob)
        .arg("600")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    // Wait for the child to report it has wired the pages.
    let child_out = child.stdout.take().expect("piped");
    let mut reader = std::io::BufReader::new(child_out);
    let mut first = String::new();
    {
        use std::io::BufRead;
        reader.read_line(&mut first)?;
    }
    let first = first.trim();
    if !first.starts_with("PINNED") {
        println!("[proof] child did not pin: {first}");
        let _ = child.kill();
        return Ok(());
    }
    let child_resident: f64 = first
        .split_whitespace()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    // Measure wired memory now that the CHILD holds the lock.
    let w1 = wired_bytes();
    let delta = w1.saturating_sub(w0);
    println!("[proof] wired with child lock (W1) : {:.0} MB", mb(w1));
    println!(
        "[proof]   Δ wired (W1−W0)           : {:.0} MB   (blob is {:.0} MB)",
        mb(delta),
        blob_mb
    );

    // Observer mapping in THIS (parent) process: map the same inode and ask
    // mincore how many pages are resident. The child faulted+wired them; the
    // parent's independent mapping resolves to those same shared UBC pages.
    let observer = PinnedRegionObserver::map(&blob, page)?;
    let resident = observer.resident_fraction();
    println!(
        "[proof] observer mincore (this proc): {:.1}% of pages resident (child reported {:.1}%)",
        resident * 100.0,
        child_resident * 100.0
    );

    // Release: close the child's stdin so it munlocks and exits, then confirm
    // wired memory falls back. ΔW appearing AND disappearing with the child is
    // what makes this impossible to attribute to anything but the child's mlock.
    drop(child.stdin.take());
    let _ = child.wait();
    // Give the kernel a beat to reclaim wired accounting.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let w2 = wired_bytes();
    println!("[proof] wired after child exit (W2): {:.0} MB", mb(w2));
    println!(
        "[proof]   Δ released (W1−W2)         : {:.0} MB",
        mb(w1.saturating_sub(w2))
    );
    println!();

    // Verdict.
    let wired_ok = delta as f64 >= blob_bytes as f64 * 0.80; // ≥80% of blob wired
    let released_ok = w1.saturating_sub(w2) as f64 >= blob_bytes as f64 * 0.60;
    let resident_ok = resident >= 0.95;
    if wired_ok && resident_ok {
        println!(
            "VERDICT: PROVEN (mechanism). A separate process wired {:.0} MB of {proof_tag}'s \
             weight blob; this process's independent mapping sees {:.0}% of those pages resident \
             without faulting them from disk{}. Cross-process UBC pinning works. NOTE: whether \
             this ELIMINATES the keepalive cold-loads depends on the idle-pressure discriminator \
             — until then the daemon runs pin+keepalive (residency.mode = pin_keepalive), not \
             pin-alone.",
            mb(delta),
            resident * 100.0,
            if released_ok {
                ", and the wire released cleanly on the pinner's exit"
            } else {
                ""
            }
        );
    } else {
        println!(
            "VERDICT: INCONCLUSIVE on this run (wired_ok={wired_ok}, resident_ok={resident_ok}, \
             released_ok={released_ok}). Δwired={:.0}MB resident={:.1}%. Re-run; if persistent, \
             the mlock wire limit or a cache-warm blob may be masking the signal.",
            mb(delta),
            resident * 100.0
        );
    }
    Ok(())
}

/// Read-only observer mapping used by the proof to call `mincore` from the
/// parent process. Separate from `PinnedRegion` because it must NOT mlock (we're
/// observing the child's wire, not adding our own).
struct PinnedRegionObserver {
    addr: *mut libc::c_void,
    len: usize,
    page: usize,
}

impl PinnedRegionObserver {
    fn map(path: &Path, page: usize) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // SAFETY: read-only mmap of an open fd; mapping outlives the fd.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { addr, len, page })
    }

    fn resident_fraction(&self) -> f64 {
        let pages = self.len.div_ceil(self.page);
        let mut vec = vec![0u8; pages];
        // SAFETY: addr/len are our mapping; vec sized one byte per page.
        let rc = unsafe {
            libc::mincore(
                self.addr,
                self.len,
                vec.as_mut_ptr() as *mut std::os::raw::c_char,
            )
        };
        if rc != 0 {
            return f64::NAN;
        }
        vec.iter().filter(|b| *b & MINCORE_INCORE != 0).count() as f64 / pages as f64
    }
}

impl Drop for PinnedRegionObserver {
    fn drop(&mut self) {
        // SAFETY: our mapping; never locked.
        unsafe {
            libc::munmap(self.addr, self.len);
        }
    }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// System-wide wired (non-reclaimable) memory in bytes, from `vm_stat`.
/// "Pages wired down: N" × page size. Used by the proof harness only.
fn wired_bytes() -> usize {
    let out = match std::process::Command::new("vm_stat").output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut page = 4096usize;
    let mut wired_pages = 0usize;
    for line in text.lines() {
        if let Some(idx) = line.find("page size of") {
            // "...page size of 16384 bytes)"
            if let Some(n) = line[idx..]
                .split_whitespace()
                .find_map(|t| t.parse::<usize>().ok())
            {
                page = n;
            }
        } else if let Some(rest) = line.strip_prefix("Pages wired down:") {
            wired_pages = rest
                .trim()
                .trim_end_matches('.')
                .parse::<usize>()
                .unwrap_or(0);
        }
    }
    wired_pages * page
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// mlock a real temp file and prove via mincore that its pages are resident.
    /// Drop releases the wire. This exercises the exact production lock path.
    #[test]
    fn pin_temp_file_makes_pages_resident_then_releases() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // 4 MB of real bytes so there are many pages to check.
        let buf = vec![0xABu8; 4 * 1024 * 1024];
        tmp.write_all(&buf).unwrap();
        tmp.flush().unwrap();

        let region = PinnedRegion::map_and_lock(tmp.path())
            .expect("mlock of a 4 MB temp file should succeed");
        let frac = region.resident_fraction();
        assert!(
            frac >= 0.99,
            "an mlock'd region must be ~100% resident, got {:.3}",
            frac
        );
        drop(region); // must not panic; munlock+munmap
    }

    #[test]
    fn pin_zero_length_file_is_rejected() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        match PinnedRegion::map_and_lock(tmp.path()) {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            Ok(_) => panic!("zero-length blob must be rejected"),
        }
    }

    #[test]
    fn resolve_model_blob_reads_real_manifest_shape() {
        // Build a synthetic Ollama store matching the on-disk layout and prove
        // the resolver finds the model layer's blob.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let man = dir.join("manifests/registry.ollama.ai/library/testmodel/7b");
        std::fs::create_dir_all(man.parent().unwrap()).unwrap();
        std::fs::write(
            &man,
            r#"{"layers":[
                {"mediaType":"application/vnd.ollama.image.license","digest":"sha256:dead","size":10},
                {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:beefcafe","size":42}
            ]}"#,
        )
        .unwrap();
        let blob = dir.join("blobs/sha256-beefcafe");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"weights").unwrap();

        let mgr = ResidencyManager::new(dir.to_path_buf());
        let resolved = mgr
            .resolve_model_blob("testmodel:7b")
            .expect("should resolve");
        assert_eq!(resolved, blob);
    }

    #[test]
    fn resolve_model_blob_missing_manifest_returns_none() {
        let root = tempfile::tempdir().unwrap();
        let mgr = ResidencyManager::new(root.path().to_path_buf());
        assert!(mgr.resolve_model_blob("nope:1b").is_none());
    }

    #[test]
    fn tensor_shard_manifest_is_not_treated_as_missing_gguf_blob() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let man = dir.join("manifests/registry.ollama.ai/library/mlx/26b");
        std::fs::create_dir_all(man.parent().unwrap()).unwrap();
        std::fs::write(
            &man,
            r#"{"layers":[
                {"mediaType":"application/vnd.ollama.image.tensor","digest":"sha256:aa","size":123},
                {"mediaType":"application/vnd.ollama.image.json","digest":"sha256:bb","size":456}
            ]}"#,
        )
        .unwrap();

        let mgr = ResidencyManager::new(dir.to_path_buf());
        assert!(mgr.resolve_model_blob("mlx:26b").is_none());
        assert!(matches!(
            mgr.resolve_model_blob_result("mlx:26b"),
            ModelBlobResolution::TensorShards
        ));
    }

    #[test]
    fn resolve_defaults_tag_to_latest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let man = dir.join("manifests/registry.ollama.ai/library/embed/latest");
        std::fs::create_dir_all(man.parent().unwrap()).unwrap();
        std::fs::write(
            &man,
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:aa","size":1}]}"#,
        )
        .unwrap();
        let blob = dir.join("blobs/sha256-aa");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"w").unwrap();

        let mgr = ResidencyManager::new(dir.to_path_buf());
        assert!(
            mgr.resolve_model_blob("embed").is_some(),
            "bare tag must default to :latest"
        );
    }

    #[test]
    fn manager_pin_unpin_lifecycle_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let man = dir.join("manifests/registry.ollama.ai/library/m/1b");
        std::fs::create_dir_all(man.parent().unwrap()).unwrap();
        std::fs::write(
            &man,
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:cc","size":1}]}"#,
        )
        .unwrap();
        let blob = dir.join("blobs/sha256-cc");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, vec![1u8; 64 * 1024]).unwrap();

        let mgr = ResidencyManager::new(dir.to_path_buf());
        assert!(!mgr.is_primary_pinned());
        assert!(mgr.pin_primary("m:1b"));
        assert!(mgr.is_primary_pinned());
        assert!(mgr.pin_primary("m:1b")); // re-pin replaces, still true
        assert!(mgr.is_primary_pinned());
        mgr.unpin_primary();
        assert!(!mgr.is_primary_pinned());
        mgr.unpin_primary(); // double-unpin is a no-op
    }

    #[test]
    fn status_reflects_pin_state_and_wired_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let man = dir.join("manifests/registry.ollama.ai/library/m/1b");
        std::fs::create_dir_all(man.parent().unwrap()).unwrap();
        std::fs::write(
            &man,
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:dd","size":1}]}"#,
        )
        .unwrap();
        let blob = dir.join("blobs/sha256-dd");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        let size = 128 * 1024;
        std::fs::write(&blob, vec![7u8; size]).unwrap();

        let mgr = ResidencyManager::new(dir.to_path_buf());
        let before = mgr.status();
        assert!(!before.primary_pinned);
        assert_eq!(before.primary_wired_bytes, 0);
        assert!(!before.lock_poisoned);

        assert!(mgr.pin_primary("m:1b"));
        let after = mgr.status();
        assert!(after.primary_pinned);
        assert_eq!(after.primary_wired_bytes, size);
        assert!(!after.lock_poisoned);

        mgr.unpin_primary();
        assert!(!mgr.status().primary_pinned);
    }
}
