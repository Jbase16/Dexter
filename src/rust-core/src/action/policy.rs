/// PolicyEngine — classifies an ActionSpec into the appropriate ActionCategory.
///
/// ## Category semantics (from IMPLEMENTATION_PLAN.md §2.2.10)
///
/// - SAFE        — execute immediately, no confirmation, no audit
/// - CAUTIOUS    — execute immediately, write audit log entry
/// - DESTRUCTIVE — require explicit operator confirmation before execution
///
/// ## Override rule
///
/// A model-specified `category_override: "destructive"` is always respected
/// (upward override). A model-specified `"safe"` or `"cautious"` on a
/// DESTRUCTIVE-classified spec is silently ignored — policy wins downward.
/// This prevents the model from accidentally (or adversarially) lowering the
/// gate on a destructive command.
use std::path::Path;

use crate::ipc::proto::ActionCategory;

use super::engine::{ActionSpec, BrowserActionKind};

// ── Classification tables ──────────────────────────────────────────────────

/// Shell commands that immediately classify as DESTRUCTIVE, regardless of arguments.
///
/// Conservative: `curl` and `wget` are included because they can write files and
/// exfiltrate data. An operator who wants to curl a URL can approve the action.
const SHELL_DESTRUCTIVE_CMDS: &[&str] = &[
    "rm", "rmdir", "sudo", "su", "chmod", "chown",
    // Phase 37 / B10: `pkill` behaves like `kill`/`killall` — signal-send
    // by process name or regex. Omitting it from this list while listing
    // its siblings was a classification bug, not a policy choice.
    "kill", "killall", "pkill", "shutdown", "reboot", "mkfs",
    "dd", "mv", "curl", "wget",
];

/// Interpreters and command-prefixers that, by their very job, can execute arbitrary
/// payloads supplied as arguments — bypassing argv[0]-based classification.
///
/// Phase 38 / Codex finding [1]: previously `["bash","-c","rm -rf ~"]` classified
/// as Cautious because only argv[0] (`bash`) was checked against destructive list.
/// Same hole for `python3 -c`, `osascript -e`, `env VAR=val rm`, `xargs rm`,
/// `find / -exec rm {} \;`, `awk 'BEGIN{system("rm -rf /")}'`. All of these are
/// signal amplifiers — any classification weaker than Destructive lets the model
/// route around the policy gate by wrapping its real intent in a wrapper command.
///
/// Approval cost is acceptable: an operator who genuinely wants to run an
/// interpreter command can approve once, and routine non-interpreter commands
/// (the common case) are unaffected.
const SHELL_INTERPRETER_CMDS: &[&str] = &[
    // POSIX shells
    "bash", "sh", "zsh", "fish", "dash", "ksh",
    // Scripting languages with -c / -e arbitrary-execution flags
    "python", "python3", "python2", "ruby", "perl", "php", "node", "deno", "lua",
    // macOS-specific arbitrary-execution
    "osascript", "swift", "swiftc",
    // Wrappers that prefix or amplify another command's privileges/effects
    "env", "exec", "xargs", "tee",
    // Iterators that spawn arbitrary commands via -exec / -execdir
    "find",
    // BEGIN{system(...)} and the `e` command can shell out
    "awk", "gawk", "nawk",
];

/// Shell commands that classify as SAFE (read-only, no observable side effects).
const SHELL_SAFE_CMDS: &[&str] = &[
    "echo", "pwd", "date", "whoami", "hostname",
    "uname", "uptime", "df", "ls", "cat",
    "head", "tail", "wc",
];

/// Substrings that, if present in an AppleScript, escalate it to DESTRUCTIVE.
///
/// Phase 38 / Codex finding [2]: previously every AppleScript classified as
/// Cautious — meaning a script containing `do shell script "rm -rf ~"` ran
/// without operator approval. AppleScript is a side-effect language with full
/// system access (Finder delete, System Events keystroke/click, do shell script
/// out to bash). Content-aware classification catches the obvious destructive
/// patterns; benign scripts (`tell application "Finder" to activate`) remain
/// Cautious.
///
/// All matching is case-insensitive (AppleScript keywords are case-insensitive).
/// The leading whitespace/newline/tab variants ensure we match the verb at a
/// statement boundary, not as a fragment of an identifier or string literal.
///
/// Deliberately NOT included: `tell application "messages"`. Self-send is now
/// Rust-rewritten via the Phase 37.9 intercept, and named-recipient sends are
/// the next phase (38b structured imessage:send action). Adding Messages to
/// this list now would require approval for every iMessage send including ones
/// the operator routinely uses.
const APPLESCRIPT_DESTRUCTIVE_MARKERS: &[&str] = &[
    "do shell script",         // Direct shell execution from AppleScript
    " keystroke ",             // System Events keystroke — drives any focused app
    "\nkeystroke ",
    "\tkeystroke ",
    " key code ",              // System Events key code (modifiers + non-printables)
    "\nkey code ",
    "\tkey code ",
    " click ",                 // UI click via System Events (delete buttons, etc.)
    "\nclick ",
    "\tclick ",
    " delete ",                // Finder delete, Mail delete, etc.
    "\ndelete ",
    "\tdelete ",
    "set the clipboard",       // Clipboard manipulation — credential exfil vector
];

/// Path prefixes where a FileWrite classifies as DESTRUCTIVE.
///
/// These are system-owned directories where writing without intent would be
/// genuinely dangerous. `/tmp` and user home directories are CAUTIOUS, not listed here.
const SYSTEM_PATH_PREFIXES: &[&str] = &[
    "/etc/", "/usr/", "/bin/", "/sbin/",
    "/System/", "/Library/",
    "/private/etc/", "/private/var/",
];

// ── PolicyEngine ──────────────────────────────────────────────────────────────

pub struct PolicyEngine;

impl PolicyEngine {
    /// Classify an ActionSpec. Returns the final category after applying any override.
    pub fn classify(spec: &ActionSpec) -> ActionCategory {
        match spec {
            ActionSpec::Shell { args, category_override, .. } => {
                let base = Self::classify_shell(args);
                Self::apply_override(base, category_override.as_deref())
            }
            ActionSpec::FileRead { .. } => {
                // Reading a file is always SAFE — no state is modified.
                ActionCategory::Safe
            }
            ActionSpec::FileWrite { path, category_override, .. } => {
                let base = Self::classify_file_write(path);
                Self::apply_override(base, category_override.as_deref())
            }
            ActionSpec::AppleScript { script, .. } => {
                // Phase 38 / Codex finding [2]: classify by content. Scripts
                // containing `do shell script`, `keystroke`, `click`, `delete`,
                // etc. escalate to Destructive; benign scripts stay Cautious.
                Self::classify_applescript(script)
            }
            ActionSpec::Browser { action, category_override, .. } => {
                let base = Self::classify_browser(action);
                Self::apply_override(base, category_override.as_deref())
            }
        }
    }

    fn classify_browser(action: &BrowserActionKind) -> ActionCategory {
        match action {
            // Read-only operations — no observable side effects on the page or disk
            // (screenshot saves to /tmp/ but is intentionally non-destructive).
            BrowserActionKind::Extract { .. }  => ActionCategory::Safe,
            BrowserActionKind::Screenshot      => ActionCategory::Safe,
            // State-changing but reversible — model uses category_override: "destructive"
            // for consequential clicks (delete account, confirm purchase, etc.).
            BrowserActionKind::Navigate { .. } => ActionCategory::Cautious,
            BrowserActionKind::Click { .. }    => ActionCategory::Cautious,
            BrowserActionKind::Type { .. }     => ActionCategory::Cautious,
        }
    }

    fn classify_shell(args: &[String]) -> ActionCategory {
        let cmd = match args.first() {
            Some(c) => c.as_str(),
            None    => return ActionCategory::Cautious,
        };
        // Strip any path prefix so "/usr/bin/rm" matches "rm".
        let base_cmd = Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(cmd);

        if SHELL_DESTRUCTIVE_CMDS.contains(&base_cmd)  { return ActionCategory::Destructive; }
        // Phase 38 / Codex finding [1]: interpreters and command-prefixers escalate
        // to Destructive — they can host arbitrary destructive payloads in args
        // (`bash -c "rm -rf"`, `python3 -c "..."`, `xargs rm`, etc.) that argv[0]
        // classification alone would let through as Cautious.
        if SHELL_INTERPRETER_CMDS.contains(&base_cmd)  { return ActionCategory::Destructive; }
        if SHELL_SAFE_CMDS.contains(&base_cmd)         { return ActionCategory::Safe; }
        ActionCategory::Cautious
    }

    /// Phase 38 / Codex finding [2]: AppleScript content classifier.
    ///
    /// Scans the (case-insensitive) script for any `APPLESCRIPT_DESTRUCTIVE_MARKERS`
    /// substring. Any match → Destructive (operator approval required). No match →
    /// Cautious (executes immediately, audit-logged) — same as the previous default.
    fn classify_applescript(script: &str) -> ActionCategory {
        let lower = script.to_ascii_lowercase();
        if APPLESCRIPT_DESTRUCTIVE_MARKERS.iter().any(|m| lower.contains(m)) {
            ActionCategory::Destructive
        } else {
            ActionCategory::Cautious
        }
    }

    fn classify_file_write(path: &Path) -> ActionCategory {
        // Phase 38 / Codex finding [3]: classify the NORMALIZED path so the
        // category matches what the executor will actually write to. Without
        // normalization, `~/../../etc/hosts` was misclassified as Cautious
        // (no system prefix) but executed against `/etc/hosts`. The same
        // normalizer is used by `execute_file_write` in `action::executor`.
        let normalized = crate::action::executor::normalize_for_policy(path);
        let path_str = normalized.to_string_lossy();
        if SYSTEM_PATH_PREFIXES.iter().any(|p| path_str.starts_with(p)) {
            ActionCategory::Destructive
        } else {
            ActionCategory::Cautious
        }
    }

    /// Apply a model-specified category override.
    ///
    /// Only upward overrides are accepted:
    ///   - `"destructive"` always wins — model can escalate any category.
    ///   - `"cautious"` upgrades SAFE → CAUTIOUS only (not DESTRUCTIVE → CAUTIOUS).
    ///   - `"safe"` is always ignored — downgrading is not permitted.
    ///   - Unknown strings are silently ignored.
    fn apply_override(base: ActionCategory, override_str: Option<&str>) -> ActionCategory {
        match override_str {
            Some("destructive") => ActionCategory::Destructive,
            Some("cautious") if base == ActionCategory::Safe => ActionCategory::Cautious,
            _ => base,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::engine::{ActionSpec, BrowserActionKind};

    fn shell(args: &[&str]) -> ActionSpec {
        ActionSpec::Shell {
            args:              args.iter().map(|s| s.to_string()).collect(),
            working_dir:       None,
            rationale:         None,
            category_override: None,
        }
    }

    fn shell_with_override(args: &[&str], override_val: &str) -> ActionSpec {
        ActionSpec::Shell {
            args:              args.iter().map(|s| s.to_string()).collect(),
            working_dir:       None,
            rationale:         None,
            category_override: Some(override_val.to_string()),
        }
    }

    fn file_write(path: &str) -> ActionSpec {
        ActionSpec::FileWrite {
            path:              std::path::PathBuf::from(path),
            content:           "data".to_string(),
            create_dirs:       false,
            rationale:         None,
            category_override: None,
        }
    }

    #[test]
    fn classify_shell_echo_is_safe() {
        assert_eq!(PolicyEngine::classify(&shell(&["echo", "hi"])), ActionCategory::Safe);
    }

    #[test]
    fn classify_shell_ls_is_safe() {
        assert_eq!(PolicyEngine::classify(&shell(&["ls", "-la"])), ActionCategory::Safe);
    }

    #[test]
    fn classify_shell_rm_is_destructive() {
        assert_eq!(PolicyEngine::classify(&shell(&["rm", "-rf", "/tmp/x"])), ActionCategory::Destructive);
    }

    #[test]
    fn classify_shell_mv_is_destructive() {
        assert_eq!(PolicyEngine::classify(&shell(&["mv", "a", "b"])), ActionCategory::Destructive);
    }

    #[test]
    fn classify_shell_pkill_is_destructive() {
        // Phase 37 / B10: pkill has the same semantics as kill/killall (signal-send
        // by pattern). It must classify at the same tier as its siblings.
        assert_eq!(PolicyEngine::classify(&shell(&["pkill", "-f", "node"])), ActionCategory::Destructive);
        assert_eq!(PolicyEngine::classify(&shell(&["/usr/bin/pkill", "chrome"])), ActionCategory::Destructive);
    }

    #[test]
    fn classify_shell_absolute_path_rm_is_destructive() {
        // /usr/bin/rm should match "rm" after stripping the path prefix.
        assert_eq!(PolicyEngine::classify(&shell(&["/usr/bin/rm", "-f", "/tmp/x"])), ActionCategory::Destructive);
    }

    #[test]
    fn classify_shell_git_is_cautious() {
        // git is not in either list → CAUTIOUS (unknown command, not obviously dangerous)
        assert_eq!(PolicyEngine::classify(&shell(&["git", "status"])), ActionCategory::Cautious);
    }

    #[test]
    fn classify_shell_empty_args_is_cautious() {
        assert_eq!(PolicyEngine::classify(&shell(&[])), ActionCategory::Cautious);
    }

    #[test]
    fn classify_shell_override_upward_to_destructive() {
        // echo is SAFE, but model can escalate to DESTRUCTIVE
        assert_eq!(
            PolicyEngine::classify(&shell_with_override(&["echo", "hi"], "destructive")),
            ActionCategory::Destructive,
        );
    }

    #[test]
    fn classify_shell_override_downward_rejected() {
        // rm is DESTRUCTIVE — model cannot downgrade to "safe" or "cautious"
        assert_eq!(
            PolicyEngine::classify(&shell_with_override(&["rm", "-rf", "/tmp/x"], "safe")),
            ActionCategory::Destructive,
        );
        assert_eq!(
            PolicyEngine::classify(&shell_with_override(&["rm", "-rf", "/tmp/x"], "cautious")),
            ActionCategory::Destructive,
        );
    }

    #[test]
    fn classify_file_read_is_safe() {
        assert_eq!(
            PolicyEngine::classify(&ActionSpec::FileRead {
                path: std::path::PathBuf::from("/Users/jason/notes.txt"),
            }),
            ActionCategory::Safe,
        );
    }

    #[test]
    fn classify_file_write_tmp_is_cautious() {
        assert_eq!(PolicyEngine::classify(&file_write("/tmp/dexter-output.txt")), ActionCategory::Cautious);
    }

    #[test]
    fn classify_file_write_home_is_cautious() {
        assert_eq!(PolicyEngine::classify(&file_write("/Users/jason/notes.txt")), ActionCategory::Cautious);
    }

    #[test]
    fn classify_file_write_etc_is_destructive() {
        assert_eq!(PolicyEngine::classify(&file_write("/etc/hosts")), ActionCategory::Destructive);
    }

    #[test]
    fn classify_file_write_system_is_destructive() {
        assert_eq!(PolicyEngine::classify(&file_write("/System/Library/test")), ActionCategory::Destructive);
    }

    #[test]
    fn classify_applescript_is_cautious() {
        assert_eq!(
            PolicyEngine::classify(&ActionSpec::AppleScript {
                script:    "tell application \"Finder\" to activate".to_string(),
                rationale: None,
            }),
            ActionCategory::Cautious,
        );
    }

    // ── Browser policy tests ──────────────────────────────────────────────────

    fn browser_extract() -> ActionSpec {
        ActionSpec::Browser {
            action:            BrowserActionKind::Extract { selector: None },
            rationale:         None,
            category_override: None,
        }
    }

    fn browser_screenshot() -> ActionSpec {
        ActionSpec::Browser {
            action:            BrowserActionKind::Screenshot,
            rationale:         None,
            category_override: None,
        }
    }

    fn browser_navigate() -> ActionSpec {
        ActionSpec::Browser {
            action:            BrowserActionKind::Navigate { url: "https://example.com".to_string() },
            rationale:         None,
            category_override: None,
        }
    }

    fn browser_click_destructive() -> ActionSpec {
        ActionSpec::Browser {
            action:            BrowserActionKind::Click { selector: "#delete-account".to_string() },
            rationale:         None,
            category_override: Some("destructive".to_string()),
        }
    }

    #[test]
    fn classify_browser_extract_is_safe() {
        assert_eq!(PolicyEngine::classify(&browser_extract()), ActionCategory::Safe);
    }

    #[test]
    fn classify_browser_screenshot_is_safe() {
        assert_eq!(PolicyEngine::classify(&browser_screenshot()), ActionCategory::Safe);
    }

    #[test]
    fn classify_browser_navigate_is_cautious() {
        assert_eq!(PolicyEngine::classify(&browser_navigate()), ActionCategory::Cautious);
    }

    #[test]
    fn classify_browser_click_with_destructive_override() {
        assert_eq!(PolicyEngine::classify(&browser_click_destructive()), ActionCategory::Destructive);
    }

    // ── Phase 38 / Codex finding [1]: shell interpreter classification ────────

    #[test]
    fn classify_shell_bash_dash_c_is_destructive() {
        // Without the interpreter list, `["bash","-c","rm -rf ~"]` would have
        // classified as Cautious (bash isn't in destructive or safe lists).
        assert_eq!(
            PolicyEngine::classify(&shell(&["bash", "-c", "rm -rf ~"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_python_dash_c_is_destructive() {
        assert_eq!(
            PolicyEngine::classify(&shell(&["python3", "-c", "import os; os.system('rm -rf /')"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_osascript_dash_e_is_destructive() {
        // The `apple_script` ActionSpec variant has its own classifier; this
        // tests the explicit `osascript` shell invocation, which used to bypass
        // the policy gate entirely by pretending to be just another binary.
        assert_eq!(
            PolicyEngine::classify(&shell(&["osascript", "-e", "do shell script \"rm -rf ~\""])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_env_prefix_is_destructive() {
        // `env VAR=val rm -rf ~` previously hid the rm under env's argv[0].
        assert_eq!(
            PolicyEngine::classify(&shell(&["env", "FOO=bar", "rm", "-rf", "/tmp/x"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_xargs_is_destructive() {
        assert_eq!(
            PolicyEngine::classify(&shell(&["xargs", "rm"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_find_is_destructive() {
        // find -exec is a known interpreter-equivalent (`-exec rm {} \;`).
        assert_eq!(
            PolicyEngine::classify(&shell(&["find", "/tmp", "-exec", "rm", "{}", ";"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_awk_system_is_destructive() {
        assert_eq!(
            PolicyEngine::classify(&shell(&["awk", "BEGIN{system(\"rm -rf /\")}"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_node_is_destructive() {
        assert_eq!(
            PolicyEngine::classify(&shell(&["node", "-e", "require('fs').rmSync('~', {recursive:true})"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_absolute_path_bash_is_destructive() {
        // /bin/bash should match "bash" after stripping the path prefix.
        assert_eq!(
            PolicyEngine::classify(&shell(&["/bin/bash", "-c", "echo hi"])),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_shell_tee_is_destructive() {
        // tee writes its stdin to a file — classified as destructive because
        // a model-driven `["tee","/etc/something"]` would write to system path.
        assert_eq!(
            PolicyEngine::classify(&shell(&["tee", "/tmp/output"])),
            ActionCategory::Destructive
        );
    }

    // ── Phase 38 / Codex finding [2]: AppleScript content classification ──────

    fn applescript(script: &str) -> ActionSpec {
        ActionSpec::AppleScript {
            script:    script.to_string(),
            rationale: None,
        }
    }

    #[test]
    fn classify_applescript_do_shell_script_is_destructive() {
        // The big one — `do shell script` lets AppleScript run arbitrary bash.
        assert_eq!(
            PolicyEngine::classify(&applescript("do shell script \"rm -rf ~\"")),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_do_shell_script_case_insensitive() {
        // AppleScript keywords are case-insensitive — the model might emit
        // any case. Classifier must lowercase before matching.
        assert_eq!(
            PolicyEngine::classify(&applescript("DO SHELL SCRIPT \"id\"")),
            ActionCategory::Destructive
        );
        assert_eq!(
            PolicyEngine::classify(&applescript("Do Shell Script \"id\"")),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_keystroke_is_destructive() {
        let s = "tell application \"System Events\"\n\
                 keystroke \"q\" using command down\n\
                 end tell";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_key_code_is_destructive() {
        let s = "tell application \"System Events\"\n\
                 key code 53\n\
                 end tell";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_click_is_destructive() {
        let s = "tell application \"System Events\"\n\
                 click button \"Delete\" of window 1 of process \"Finder\"\n\
                 end tell";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_finder_delete_is_destructive() {
        let s = "tell application \"Finder\"\n\
                 delete every item of folder \"Downloads\" of home\n\
                 end tell";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_set_clipboard_is_destructive() {
        // Clipboard manipulation is a credential-exfil vector.
        assert_eq!(
            PolicyEngine::classify(&applescript("set the clipboard to (do shell script \"id\")")),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_applescript_benign_activate_remains_cautious() {
        // `tell app to activate` has no destructive markers — Cautious is correct.
        assert_eq!(
            PolicyEngine::classify(&applescript("tell application \"Finder\" to activate")),
            ActionCategory::Cautious
        );
    }

    #[test]
    fn classify_applescript_messages_send_remains_cautious() {
        // Phase 38 design choice: iMessage send is NOT in the destructive markers
        // because (a) self-send is now Rust-rewritten via the Phase 37.9 intercept
        // and (b) named-recipient hardening is the Phase 38b structured
        // imessage:send action. Adding it here now would require approval for
        // every iMessage send, including ones the operator routinely uses. The
        // intercept handles the actual confabulation risk.
        let s = "tell application \"Messages\"\n\
                 set targetService to 1st service whose service type = iMessage\n\
                 set targetBuddy to buddy \"+15551234567\" of targetService\n\
                 send \"hi\" to targetBuddy\n\
                 end tell";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Cautious
        );
    }

    #[test]
    fn classify_applescript_string_containing_keystroke_word_is_destructive() {
        // Acceptable conservative-leaning false positive: a script that contains
        // " keystroke " in a string literal (e.g. a log message) still escalates
        // to Destructive. We pay this cost because correctly distinguishing
        // string-literal vs. statement-context would require an AppleScript
        // parser. Documented for future tightening.
        let s = "log \" keystroke triggered\"";
        assert_eq!(
            PolicyEngine::classify(&applescript(s)),
            ActionCategory::Destructive
        );
    }

    // ── Phase 38 / Codex finding [3]: FileWrite path normalization ─────────────

    #[test]
    fn classify_file_write_dotdot_to_etc_is_destructive() {
        // The exact bypass Codex flagged: ~/../../etc/hosts looks home-directoried
        // (Cautious by raw-path classification) but normalizes to /etc/hosts.
        assert_eq!(
            PolicyEngine::classify(&file_write("~/../../etc/hosts")),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_file_write_absolute_dotdot_to_system_is_destructive() {
        assert_eq!(
            PolicyEngine::classify(&file_write("/Users/jason/../../etc/hosts")),
            ActionCategory::Destructive
        );
    }

    #[test]
    fn classify_file_write_normalize_self_referential_remains_cautious() {
        // `~/foo/./bar/file.txt` normalizes to `~/foo/bar/file.txt` — still home,
        // still Cautious.
        assert_eq!(
            PolicyEngine::classify(&file_write("/Users/jason/foo/./bar/file.txt")),
            ActionCategory::Cautious
        );
    }

    #[test]
    fn classify_file_write_dotdot_within_home_remains_cautious() {
        // `~/projects/foo/../bar/file.txt` normalizes to `~/projects/bar/file.txt` —
        // the legitimate "navigate sibling" case. Must not over-escalate.
        assert_eq!(
            PolicyEngine::classify(&file_write("/Users/jason/projects/foo/../bar/file.txt")),
            ActionCategory::Cautious
        );
    }

    #[test]
    fn classify_file_write_etc_still_destructive_directly() {
        // Regression guard for the original test — direct system-prefix paths
        // remain Destructive after the normalization refactor.
        assert_eq!(
            PolicyEngine::classify(&file_write("/etc/hosts")),
            ActionCategory::Destructive
        );
    }
}
