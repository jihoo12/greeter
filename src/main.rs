//! greetd-mini-greeter: a minimal, dependency-light CLI greeter for greetd.
//!
//! Flow: banner -> username -> (session select, if >1 available) -> password /
//! auth prompts (relayed verbatim from the PAM stack via greetd) -> start session.
//!
//! Design goals: work with zero configuration on a freshly installed system,
//! degrade gracefully (falls back to the user's login shell if no
//! Wayland/X11 session files are found), and never leave the terminal in a
//! broken (no-echo) state.

use std::env;
use std::ffi::CStr;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};

/// Number of consecutive failed login attempts (this process only ever
/// handles one attempt at a time, so this counts across usernames too)
/// before we start adding an escalating delay on top of the normal retry
/// pause. This is only a coarse, greeter-side speed bump -- real
/// lockout/backoff policy belongs in PAM (faillock, tally2, etc) -- but it
/// keeps a misconfigured or overly permissive PAM stack from turning this
/// terminal into a fast local brute-force oracle.
const BACKOFF_AFTER_FAILURES: u32 = 3;
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Directories scanned for desktop session files, in priority order.
/// Covers NixOS (`/run/current-system/sw/...`) as well as the common FHS
/// locations used by most other distros.
const SESSION_DIRS: &[(&str, SessionKind)] = &[
    (
        "/run/current-system/sw/share/wayland-sessions",
        SessionKind::Wayland,
    ),
    ("/run/current-system/sw/share/xsessions", SessionKind::X11),
    ("/usr/share/wayland-sessions", SessionKind::Wayland),
    ("/usr/share/xsessions", SessionKind::X11),
    ("/usr/local/share/wayland-sessions", SessionKind::Wayland),
    ("/usr/local/share/xsessions", SessionKind::X11),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionKind {
    Wayland,
    X11,
}

#[derive(Clone, Debug)]
struct Session {
    name: String,
    cmd: Vec<String>,
    kind: SessionKind,
}

impl Session {
    fn env(&self) -> Vec<String> {
        match self.kind {
            SessionKind::Wayland => vec!["XDG_SESSION_TYPE=wayland".to_string()],
            SessionKind::X11 => vec!["XDG_SESSION_TYPE=x11".to_string()],
        }
    }
}

#[derive(Debug)]
enum GreeterError {
    Io(String),
    Codec(String),
}

impl From<io::Error> for GreeterError {
    fn from(e: io::Error) -> Self {
        GreeterError::Io(e.to_string())
    }
}

impl From<greetd_ipc::codec::Error> for GreeterError {
    fn from(e: greetd_ipc::codec::Error) -> Self {
        GreeterError::Codec(e.to_string())
    }
}

/// Outcome of one full login attempt.
enum LoginOutcome {
    /// Session was started; the greeter should now exit and let greetd
    /// hand control to the session.
    Started,
    /// Authentication or session start failed; message to show the user
    /// before looping back to the username prompt.
    Retry(String),
}

fn main() {
    // Safety net: if a previous instance of this process was killed
    // mid-prompt (e.g. by a signal) the tty may have been left with ECHO
    // disabled. Force it back on before we do anything else.
    set_echo(true);

    let sock_path = match env::var("GREETD_SOCK") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "error: GREETD_SOCK is not set.\n\
                 This program is a greetd greeter and must be launched by greetd \
                 (see the `command` setting in greetd's config.toml)."
            );
            std::process::exit(1);
        }
    };

    let sessions = discover_sessions();

    // Consecutive failed attempts, reset on any successful CreateSession
    // (i.e. correct credentials). Drives the escalating backoff below.
    let mut consecutive_failures: u32 = 0;

    loop {
        clear_screen();
        print_banner();

        let username = match prompt_line("login: ") {
            Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
            Ok(_) => continue,
            Err(_) => {
                // EOF (Ctrl-D) or unreadable terminal: exit and let greetd
                // decide whether to respawn us.
                println!();
                std::process::exit(0);
            }
        };

        let chosen = match select_session(&sessions) {
            Some(s) => s,
            None => continue,
        };

        match attempt_login(&sock_path, &username, chosen) {
            Ok(LoginOutcome::Started) => {
                // greetd now owns the session; nothing more for us to do.
                std::process::exit(0);
            }
            Ok(LoginOutcome::Retry(msg)) => {
                set_echo(true);
                println!("\n{}", msg);
                consecutive_failures = consecutive_failures.saturating_add(1);
                std::thread::sleep(backoff_delay(consecutive_failures));
            }
            Err(e) => {
                set_echo(true);
                eprintln!(
                    "\ncould not talk to greetd: {}",
                    match e {
                        GreeterError::Io(m) => m,
                        GreeterError::Codec(m) => m,
                    }
                );
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Base retry pause plus an escalating penalty once several consecutive
/// attempts have failed. Deliberately coarse -- this is a local speed bump,
/// not a substitute for PAM-level lockout policy.
fn backoff_delay(consecutive_failures: u32) -> Duration {
    let base = Duration::from_millis(1200);
    if consecutive_failures < BACKOFF_AFTER_FAILURES {
        return base;
    }
    let extra_steps = consecutive_failures - BACKOFF_AFTER_FAILURES + 1;
    let extra = Duration::from_millis(800).saturating_mul(extra_steps.min(20));
    (base + extra).min(MAX_BACKOFF)
}

/// One selected session to log into: either a discovered desktop entry, or
/// the "just start my login shell" fallback used when none are found.
enum Choice<'a> {
    Desktop(&'a Session),
    Shell,
}

fn select_session(sessions: &[Session]) -> Option<Choice<'_>> {
    match sessions.len() {
        0 => Some(Choice::Shell),
        1 => {
            println!("session: {}", sessions[0].name);
            Some(Choice::Desktop(&sessions[0]))
        }
        _ => {
            println!("available sessions:");
            for (i, s) in sessions.iter().enumerate() {
                println!("  {}) {}", i + 1, s.name);
            }
            let raw = match prompt_line(&format!("session [1-{}, default 1]: ", sessions.len())) {
                Ok(s) => s,
                Err(_) => return None,
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Some(Choice::Desktop(&sessions[0]));
            }
            match trimmed.parse::<usize>() {
                Ok(n) if n >= 1 && n <= sessions.len() => Some(Choice::Desktop(&sessions[n - 1])),
                _ => {
                    println!("not a valid choice, try again");
                    None
                }
            }
        }
    }
}

fn attempt_login(
    sock_path: &str,
    username: &str,
    choice: Choice,
) -> Result<LoginOutcome, GreeterError> {
    let mut stream = UnixStream::connect(sock_path)?;

    Request::CreateSession {
        username: username.to_string(),
    }
    .write_to(&mut stream)?;

    loop {
        match Response::read_from(&mut stream)? {
            Response::Success => break,
            Response::Error {
                error_type,
                description,
            } => {
                // Best-effort: greetd cancels automatically on error, but a
                // dangling CancelSession never hurts.
                let _ = Request::CancelSession.write_to(&mut stream);
                let msg = match error_type {
                    ErrorType::AuthError => "Login incorrect".to_string(),
                    ErrorType::Error => {
                        // The tty is visible to anyone at the physical
                        // console, so don't echo greetd's raw error
                        // description there -- it may contain internal
                        // paths or config detail. Full detail still goes
                        // to stderr (journal/log), just not the screen.
                        eprintln!("greetd error: {}", description);
                        "an internal error occurred, see system logs".to_string()
                    }
                };
                return Ok(LoginOutcome::Retry(msg));
            }
            Response::AuthMessage {
                auth_message_type,
                auth_message,
            } => {
                let reply = match auth_message_type {
                    AuthMessageType::Visible => Some(prompt_line(&format!("{} ", auth_message))?),
                    AuthMessageType::Secret => {
                        // greetd_ipc's Request takes an owned String, so we
                        // can't avoid handing it one copy of the plaintext.
                        // We zero *our* Secret buffer as soon as we're done
                        // reading from it, but the copy handed to the
                        // Request is scrubbed separately below, right after
                        // it's been written to the socket -- see there for
                        // why that's necessary.
                        let secret = prompt_password(&format!("{} ", auth_message))?;
                        let copy_for_request = secret.as_str().to_string();
                        drop(secret); // zeroes our buffer now, not whenever the allocator feels like it
                        Some(copy_for_request)
                    }
                    AuthMessageType::Info => {
                        println!("{}", auth_message);
                        None
                    }
                    AuthMessageType::Error => {
                        eprintln!("{}", auth_message);
                        None
                    }
                };
                let is_secret_reply = matches!(auth_message_type, AuthMessageType::Secret);
                let mut request = Request::PostAuthMessageResponse { response: reply };
                request.write_to(&mut stream)?;
                // The Request enum now owns the plaintext copy made above.
                // write_to() only needs `&self`, so we still hold it here --
                // scrub it before it's dropped, rather than letting an
                // ordinary (non-zeroing) String::drop free it with the
                // plaintext still sitting in the freed heap block.
                if is_secret_reply {
                    if let Request::PostAuthMessageResponse {
                        response: Some(ref mut s),
                    } = request
                    {
                        zeroize_string(s);
                    }
                }
            }
        }
    }

    // Authenticated. Figure out what to actually launch.
    let (cmd, env) = match choice {
        Choice::Desktop(s) => (s.cmd.clone(), s.env()),
        Choice::Shell => {
            let shell = login_shell_for(username).unwrap_or_else(|| "/bin/sh".to_string());
            (vec![shell, "-l".to_string()], vec![])
        }
    };

    Request::StartSession { cmd, env }.write_to(&mut stream)?;

    match Response::read_from(&mut stream)? {
        Response::Success => Ok(LoginOutcome::Started),
        Response::Error { description, .. } => {
            Ok(LoginOutcome::Retry(format!("could not start session: {}", description)))
        }
        Response::AuthMessage { .. } => Ok(LoginOutcome::Retry(
            "unexpected auth prompt after successful login".to_string(),
        )),
    }
}

/// Looks up the login shell for `username` via the passwd database
/// (works against nsswitch, so this also covers LDAP/systemd-homed/etc,
/// not just /etc/passwd).
fn login_shell_for(username: &str) -> Option<String> {
    let c_username = std::ffi::CString::new(username).ok()?;
    unsafe {
        let pw = libc::getpwnam(c_username.as_ptr());
        if pw.is_null() {
            return None;
        }
        let shell = (*pw).pw_shell;
        if shell.is_null() {
            return None;
        }
        CStr::from_ptr(shell).to_str().ok().map(|s| s.to_string())
    }
}

fn discover_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();

    for (dir, kind) in SESSION_DIRS {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(session) = parse_desktop_file(&path, *kind) {
                sessions.push(session);
            }
        }
    }

    sessions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    sessions.dedup_by(|a, b| a.name == b.name);
    sessions
}

fn parse_desktop_file(path: &Path, kind: SessionKind) -> Option<Session> {
    let content = fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut exec = None;
    let mut try_exec = None;
    let mut hidden = false;
    let mut no_display = false;

    for line in content.lines() {
        let line = line.trim();
        if name.is_none() {
            if let Some(v) = line.strip_prefix("Name=") {
                name = Some(v.to_string());
            }
        }
        if exec.is_none() {
            if let Some(v) = line.strip_prefix("Exec=") {
                exec = Some(v.to_string());
            }
        }
        if try_exec.is_none() {
            if let Some(v) = line.strip_prefix("TryExec=") {
                try_exec = Some(v.to_string());
            }
        }
        if line == "Hidden=true" {
            hidden = true;
        }
        if line == "NoDisplay=true" {
            no_display = true;
        }
    }

    // Per the desktop entry spec, both of these mean "don't offer this
    // entry to the user" -- respect them the same way any other launcher
    // would, rather than surfacing every .desktop file we happen to find.
    if hidden || no_display {
        return None;
    }

    // If the entry names a TryExec binary, only offer the session when
    // that binary is actually resolvable -- otherwise we'd let someone
    // pick a session that's guaranteed to fail to start.
    if let Some(bin) = &try_exec {
        if !binary_exists(bin) {
            return None;
        }
    }

    let name = name?;
    let exec_line = exec?;

    // Strip desktop-entry field codes (%f, %U, ...) which are meaningless
    // for a session command.
    let cleaned: String = exec_line
        .split_whitespace()
        .filter(|tok| !tok.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ");

    let cmd = shell_words::split(&cleaned)
        .unwrap_or_else(|_| cleaned.split_whitespace().map(str::to_string).collect());

    if cmd.is_empty() {
        return None;
    }

    Some(Session { name, cmd, kind })
}

/// Resolves `name` the way a shell would: absolute/relative paths are
/// checked directly, bare names are looked up on `$PATH`.
fn binary_exists(name: &str) -> bool {
    if name.contains('/') {
        return Path::new(name).is_file();
    }
    let path_var = match env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    env::split_paths(&path_var).any(|dir| dir.join(name).is_file())
}

fn print_banner() {
    let hostname = fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    println!("{hostname}\n");
}

fn clear_screen() {
    // ANSI clear + move cursor home. Harmless if the terminal doesn't
    // support it (worst case a couple of stray bytes).
    print!("\x1B[2J\x1B[H");
    let _ = io::stdout().flush();
}

/// Overwrites a `String`'s backing bytes with zeroes in place. Zero is
/// valid UTF-8, so this can't produce an invalid `String`. `write_volatile`
/// + a compiler fence keep the optimizer from deciding the writes are dead
/// (since nothing reads the buffer afterwards) and eliding them.
///
/// This only scrubs the allocation `s` currently points at -- it does *not*
/// retroactively clean up any earlier reallocations a growing buffer may
/// have left behind (see `prompt_password`, which preallocates capacity up
/// front specifically to avoid that).
fn zeroize_string(s: &mut String) {
    unsafe {
        for b in s.as_mut_vec() {
            std::ptr::write_volatile(b, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// A password buffer that overwrites its own backing memory with zeroes
/// when dropped, so the plaintext doesn't linger on the heap (swap, core
/// dumps, a future buffer-overread bug, etc) for any longer than
/// necessary.
struct Secret(String);

impl Secret {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize_string(&mut self.0);
    }
}

/// RAII guard that disables terminal echo for as long as it's alive and
/// restores it on drop. Unlike a bare `set_echo(false)` / `set_echo(true)`
/// pair, `Drop::drop` still runs during panic unwinding, so a panic while
/// reading the password can no longer leave the tty stuck in no-echo mode.
struct NoEchoGuard;

impl NoEchoGuard {
    fn new() -> Self {
        set_echo(false);
        NoEchoGuard
    }
}

impl Drop for NoEchoGuard {
    fn drop(&mut self) {
        set_echo(true);
    }
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    let n = io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn prompt_password(prompt: &str) -> io::Result<Secret> {
    print!("{prompt}");
    io::stdout().flush()?;
    let _guard = NoEchoGuard::new();
    // Preallocate generously so an ordinary-length password doesn't force
    // read_line to grow (and thus reallocate-and-copy) the buffer, which
    // would leave a plaintext copy behind in the old, now-freed allocation
    // that we have no handle left to zero.
    let mut line = String::with_capacity(256);
    let n = io::stdin().lock().read_line(&mut line)?;
    println!();
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
    }
    let trimmed_len = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(trimmed_len);
    Ok(Secret(line))
}

/// Toggles the ECHO flag on the controlling terminal via termios, keeping
/// canonical mode (ICANON) on so line editing (backspace, etc.) keeps
/// working exactly as normal -- only the visual echo is suppressed.
fn set_echo(enabled: bool) {
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut term) != 0 {
            eprintln!("warning: tcgetattr failed, could not read terminal attributes");
            return;
        }
        if enabled {
            term.c_lflag |= libc::ECHO;
        } else {
            term.c_lflag &= !libc::ECHO;
        }
        if libc::tcsetattr(fd, libc::TCSANOW, &term) != 0 {
            eprintln!(
                "warning: tcsetattr failed, could not {} terminal echo",
                if enabled { "restore" } else { "disable" }
            );
        }
    }
}
