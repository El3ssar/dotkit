//! Shared plumbing: where things live, output, running commands, JSON and file helpers.
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REMOTE_KIT: &str = ".local/kit"; // where `kit push` installs on servers

pub struct Ctx {
    pub home: PathBuf,
    pub source: PathBuf,
    pub tree: PathBuf,
    pub pkgs: PathBuf,
    pub rules: PathBuf,
    pub externals: PathBuf,
    pub ignore: PathBuf,
    pub dirs: PathBuf,
    pub scripts: PathBuf,
    pub remotes: PathBuf,
    pub state: PathBuf,
    pub synced: PathBuf,
    pub script_state: PathBuf,
    pub backups: PathBuf,
    pub is_mac: bool,
    pub platform: &'static str,
    pub tty: bool,
    pub interactive: bool,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static EXIT: AtomicI32 = AtomicI32::new(0);

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

pub fn expanduser(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

pub fn ctx() -> &'static Ctx {
    CTX.get_or_init(|| {
        let home = PathBuf::from(env_nonempty("HOME").unwrap_or_else(|| "/".into()));
        let source = match env_nonempty("KIT_SOURCE") {
            Some(s) => expanduser(&s, &home),
            None => home.join(".local/share/kit"),
        };
        let state = match env_nonempty("KIT_STATE") {
            Some(s) => PathBuf::from(s),
            None => PathBuf::from(
                env_nonempty("XDG_STATE_HOME")
                    .unwrap_or_else(|| home.join(".local/state").to_string_lossy().into_owned()),
            )
            .join("kit"),
        };
        let is_mac = cfg!(target_os = "macos");
        Ctx {
            tree: source.join("home"),
            pkgs: source.join("packages.json"),
            rules: source.join("rules.json"),
            externals: source.join("externals.json"),
            ignore: source.join(".kitignore"),
            dirs: source.join("dirs.json"),
            scripts: source.join("scripts"),
            remotes: source.join("remotes.json"),
            synced: state.join("synced.json"),
            script_state: state.join("scripts.json"),
            backups: state.join("backups"),
            state,
            source,
            home,
            is_mac,
            platform: if is_mac { "mac" } else { "linux" },
            tty: std::io::stdout().is_terminal(),
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    })
}

// --------------------------------------------------------------------------- output
pub fn c(code: &str, s: &str) -> String {
    if ctx().tty {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    if writeln!(o, "{s}").is_err() {
        broken_pipe();
    }
}

pub fn out_raw(s: &[u8]) {
    let mut o = std::io::stdout().lock();
    if o.write_all(s).and_then(|_| o.flush()).is_err() {
        broken_pipe();
    }
}

fn broken_pipe() -> ! {
    std::process::exit(141)
}

pub fn err_line(s: &str) {
    let _ = writeln!(std::io::stderr(), "{s}");
}

pub fn say(msg: &str) {
    out(&format!("{} {msg}", c("32", "›")));
}

pub fn note(msg: &str) {
    out(&c("2", &format!("  {msg}")));
}

pub fn warn(msg: &str) {
    err_line(&c("33", &format!("! {msg}")));
}

/// A problem that makes the command exit non-zero, but lets it finish.
pub fn fail(msg: &str) {
    err_line(&c("31", &format!("✗ {msg}")));
    EXIT.store(1, Ordering::SeqCst);
}

pub fn die(msg: &str) -> ! {
    die_code(msg, 1)
}

pub fn die_code(msg: &str, code: i32) -> ! {
    err_line(&c("31", &format!("kit: {msg}")));
    std::process::exit(code)
}

pub fn set_exit(code: i32) {
    EXIT.store(code, Ordering::SeqCst);
}

pub fn exit_code() -> i32 {
    EXIT.load(Ordering::SeqCst)
}

/// Question on a terminal; returns the lowercased first letter of the answer ("" = default).
pub fn ask(question: &str) -> String {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return String::new();
    }
    line.trim().to_lowercase().chars().take(1).collect()
}

// --------------------------------------------------------------------------- commands
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run a command. capture=false lets it talk to the terminal.
pub fn run<S: AsRef<OsStr>>(cmd: &[S], capture: bool) -> Out {
    run_full(cmd, capture, None, None, None)
}

/// Provider lookups have a bounded lifetime and cannot ask questions on stdin.
/// Their process group is owned by this call, so a timeout also stops children.
pub fn query<S: AsRef<OsStr>>(cmd: &[S], seconds: u64) -> Out {
    query_full(cmd, seconds, None, None)
}

pub fn query_full<S: AsRef<OsStr>>(
    cmd: &[S],
    seconds: u64,
    cwd: Option<&Path>,
    env: Option<&[(String, String)]>,
) -> Out {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let program = cmd[0].as_ref();
    let resolved = program.to_str().and_then(which);
    let mut command = Command::new(resolved.as_deref().map(Path::as_os_str).unwrap_or(program));
    command
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
        .env("MISE_HTTP_TIMEOUT", "20s")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(vars) = env {
        command.envs(vars.iter().map(|(k, v)| (k, v)));
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Out {
                code: 127,
                stdout: String::new(),
                stderr: e.to_string(),
            }
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let reader = |pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(8 * 1024 * 1024).read_to_end(&mut bytes);
            bytes
        })
    };
    let out = reader(Box::new(stdout));
    let err = reader(Box::new(stderr));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                // SAFETY: the child created a new process group whose id is its pid.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                break 124;
            }
        }
    };
    // A query may exit while a background child still holds its output pipe.
    // Close the owned group before joining the readers; never wait indefinitely.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let mut stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if code == 124 {
        stderr.push_str(&format!("provider lookup timed out after {seconds}s"));
    }
    if stdout.len() >= 8 * 1024 * 1024 || stderr.len() >= 8 * 1024 * 1024 {
        code = 1;
        stderr.push_str("provider response exceeds the 8 MiB limit");
    }
    Out {
        code,
        stdout,
        stderr,
    }
}

/// Installs are noninteractive, logged, and bounded. Ordinary read-only queries
/// keep using run_full so checking status does not write install state.
pub fn run_install<S: AsRef<OsStr>>(
    name: &str,
    cmd: &[S],
    cwd: Option<&Path>,
    env: Option<&[(String, String)]>,
) -> Out {
    let state = &ctx().state;
    let script = state.join("run-install.sh");
    if let Err(e) = write_atomic(
        &script,
        include_bytes!("../assets/remote/run-install.sh"),
        0o700,
    ) {
        return Out {
            code: 1,
            stdout: String::new(),
            stderr: format!("cannot prepare install runner: {e}"),
        };
    }
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let log = state.join(format!("install-{safe}.log"));
    note(&format!("install log: {}", log.display()));
    let limit = env
        .and_then(|vars| {
            vars.iter()
                .find(|(k, _)| k == "KIT_INSTALL_TIMEOUT")
                .map(|(_, v)| v.clone())
        })
        .or_else(|| std::env::var("KIT_INSTALL_TIMEOUT").ok())
        .unwrap_or_else(|| "1800".into());
    let mut args: Vec<std::ffi::OsString> = vec![
        "bash".into(),
        script.into_os_string(),
        log.into_os_string(),
        limit.into(),
    ];
    // The wrapper executes its argv directly. Match run_full/query_full's
    // executable lookup, including a freshly bootstrapped private mise or a
    // Cargo installation that is not yet on this shell's PATH.
    let program = cmd[0].as_ref();
    let resolved = program.to_str().and_then(which);
    args.push(
        resolved
            .as_deref()
            .map(Path::as_os_str)
            .unwrap_or(program)
            .to_os_string(),
    );
    args.extend(cmd[1..].iter().map(|s| s.as_ref().to_os_string()));
    run_full(&args, false, cwd, env, None)
}

pub fn run_full<S: AsRef<OsStr>>(
    cmd: &[S],
    capture: bool,
    cwd: Option<&Path>,
    env: Option<&[(String, String)]>,
    input: Option<&str>,
) -> Out {
    let program = cmd[0].as_ref();
    let resolved = program.to_str().and_then(which);
    let mut c = Command::new(resolved.as_deref().map(Path::as_os_str).unwrap_or(program));
    c.args(&cmd[1..]);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    if let Some(vars) = env {
        for (k, v) in vars {
            c.env(k, v);
        }
    }
    if capture {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    if input.is_some() {
        c.stdin(Stdio::piped());
    }
    let _ = std::io::stdout().flush();
    let child = c.spawn();
    let mut child = match child {
        Ok(ch) => ch,
        Err(_) => {
            let name = cmd[0].as_ref().to_string_lossy().into_owned();
            return Out {
                code: 127,
                stdout: String::new(),
                stderr: format!("{name}: not installed"),
            };
        }
    };
    if let Some(text) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
    }
    match child.wait_with_output() {
        Ok(o) => Out {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(_) => Out {
            code: 1,
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

/// Like run, but dies when the command fails.
pub fn run_check<S: AsRef<OsStr>>(cmd: &[S], capture: bool) -> Out {
    let r = run(cmd, capture);
    if r.code == 127 && r.stderr.ends_with("not installed") {
        die(&format!(
            "{} is not installed; install it and retry",
            cmd[0].as_ref().to_string_lossy()
        ));
    }
    if r.code != 0 {
        let detail = if capture && !r.stderr.trim().is_empty() {
            format!("\n{}", r.stderr.trim())
        } else {
            String::new()
        };
        let shown: Vec<String> = cmd
            .iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();
        die(&format!(
            "command failed ({}): {}{detail}",
            r.code,
            shown.join(" ")
        ));
    }
    r
}

pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        let p = PathBuf::from(cmd);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .chain(std::env::var_os("CARGO_HOME").map(|home| PathBuf::from(home).join("bin").join(cmd)))
        .chain([
            ctx().home.join(".local/kit/bin").join(cmd),
            ctx().home.join(".local/kit/mise/shims").join(cmd),
            ctx().home.join(".cargo/bin").join(cmd),
        ])
        .find(|p| is_executable(p))
}

pub fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn have(cmd: &str) -> bool {
    which(cmd).is_some()
}

pub fn git(args: &[&str]) -> Out {
    let src = ctx().source.to_string_lossy().into_owned();
    let mut cmd = vec!["git", "-C", &src];
    cmd.extend_from_slice(args);
    run(&cmd, true)
}

pub fn git_check(args: &[&str]) -> Out {
    let src = ctx().source.to_string_lossy().into_owned();
    let mut cmd = vec!["git", "-C", &src];
    cmd.extend_from_slice(args);
    run_check(&cmd, true)
}

// --------------------------------------------------------------------------- files
fn tmp_for(dest: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(
        ".{name}.kit-tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

pub fn write_atomic(dest: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = tmp_for(dest);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        file.write_all(data)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn link_atomic(dest: &Path, target: &str) -> std::io::Result<()> {
    let tmp = tmp_for(dest);
    std::os::unix::fs::symlink(target, &tmp)?;
    let result = fs::rename(&tmp, dest);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn exists_or_link(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

pub fn is_link(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn load_json(path: &Path, default: Value) -> Value {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default,
        Err(e) => die(&format!("could not read {}: {e}", path.display())),
        Ok(text) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => die(&format!(
                "{} is not valid JSON (line {}: {}). A merge conflict? Fix this file in the repo, then try again.",
                path.file_name().unwrap_or_default().to_string_lossy(),
                e.line(),
                e
            )),
        },
    }
}

pub fn load_obj(path: &Path) -> serde_json::Map<String, Value> {
    match load_json(path, Value::Object(Default::default())) {
        Value::Object(m) => m,
        _ => die(&format!(
            "{} must contain a JSON object",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
    }
}

pub fn load_str_list(path: &Path) -> Vec<String> {
    match load_json(path, Value::Array(vec![])) {
        Value::Array(a) => a
            .into_iter()
            .map(|v| {
                v.as_str().map(String::from).unwrap_or_else(|| {
                    die(&format!("{} must contain only strings", path.display()))
                })
            })
            .collect(),
        _ => die(&format!(
            "{} must contain a JSON list",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
    }
}

pub fn save_json(path: &Path, data: &Value) {
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    let text = serde_json::to_string_pretty(data).unwrap_or_default() + "\n";
    if let Err(e) = write_atomic(path, text.as_bytes(), 0o644) {
        die(&format!("could not write {}: {e}", path.display()));
    }
}

// --------------------------------------------------------------------------- paths
/// os.path.normpath for a relative path; "" for ".".
pub fn norm(rel: &str) -> String {
    let abs = rel.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for p in rel.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(l) if *l != "..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            x => parts.push(x),
        }
    }
    let joined = parts.join("/");
    if abs {
        format!("/{joined}")
    } else {
        joined
    }
}

pub fn under(rel: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || rel == prefix
        || (rel.starts_with(prefix) && rel[prefix.len()..].starts_with('/'))
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A path for messages: ~/… when it's inside $HOME.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(&ctx().home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn rel_to(p: &Path, base: &Path) -> Option<String> {
    p.strip_prefix(base).ok().map(path_str)
}

// --------------------------------------------------------------------------- fnmatch / glob
static FNMATCH: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();

fn translate(pat: &str, in_folder: bool) -> String {
    let any = if in_folder { "[^/]*" } else { ".*" };
    let one = if in_folder { "[^/]" } else { "." };
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut res = String::from("(?s)^");
    while i < chars.len() {
        let ch = chars[i];
        i += 1;
        match ch {
            '*' => {
                while i < chars.len() && chars[i] == '*' {
                    i += 1;
                }
                res.push_str(any)
            }
            '?' => res.push_str(one),
            '[' => {
                let mut j = i;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    res.push_str("\\[");
                } else {
                    let mut stuff: String = chars[i..j].iter().collect();
                    i = j + 1;
                    stuff = stuff.replace('\\', "\\\\");
                    if let Some(rest) = stuff.strip_prefix('!') {
                        stuff = format!("^{rest}");
                    } else if stuff.starts_with('^') {
                        stuff = format!("\\{stuff}");
                    }
                    stuff = stuff
                        .replace('[', "\\[")
                        .replace("&&", "\\&\\&")
                        .replace("~~", "\\~\\~")
                        .replace("--", "\\-\\-");
                    if stuff.is_empty() {
                        res.push_str("(?!)");
                    } else {
                        res.push('[');
                        res.push_str(&stuff);
                        res.push(']');
                    }
                }
            }
            c => res.push_str(&regex::escape(&c.to_string())),
        }
    }
    res.push('$');
    res
}

/// Python's fnmatch.fnmatchcase: * and ? also match '/'.
pub fn fnmatch(name: &str, pat: &str) -> bool {
    let cache = FNMATCH.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if !cache.contains_key(pat) {
        let re = Regex::new(&translate(pat, false))
            .unwrap_or_else(|_| Regex::new(&format!("^{}$", regex::escape(pat))).unwrap());
        cache.insert(pat.to_string(), re);
    }
    cache[pat].is_match(name)
}

/// gitignore-style match for patterns with a '/': * and ? stay inside one folder, ** crosses folders.
pub fn pathmatch(path: &str, pat: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if !cache.contains_key(pat) {
        let mut res = String::from("(?s)^");
        let mut rest = pat;
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix("**/") {
                res.push_str("(?:.*/)?");
                rest = r;
            } else if let Some(r) = rest.strip_prefix("**") {
                res.push_str(".*");
                rest = r;
            } else {
                let end = rest.find("**").unwrap_or(rest.len());
                let t = translate(&rest[..end], true);
                res.push_str(&t["(?s)^".len()..t.len() - 1]);
                rest = &rest[end..];
            }
        }
        res.push('$');
        let re = Regex::new(&res)
            .unwrap_or_else(|_| Regex::new(&format!("^{}$", regex::escape(pat))).unwrap());
        cache.insert(pat.to_string(), re);
    }
    cache[pat].is_match(path)
}

/// glob.escape: wrap * ? [ in brackets.
pub fn glob_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if matches!(ch, '*' | '?' | '[') {
            out.push('[');
            out.push(ch);
            out.push(']');
        } else {
            out.push(ch);
        }
    }
    out
}

/// A new, unused backup name (time to the millisecond; never reused even within one).
pub fn now_stamp() -> String {
    let base = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string();
    let backups = &ctx().backups;
    let mut stamp = base.clone();
    let mut n = 1;
    while backups.join(&stamp).exists() || backups.join(format!("{stamp}-undo")).exists() {
        stamp = format!("{base}{n}");
        n += 1;
    }
    std::fs::create_dir_all(backups.join(&stamp))
        .unwrap_or_else(|e| die(&format!("cannot create backup folder: {e}")));
    stamp
}

/// Serialize commands that update the same repo or machine state. flock releases on exit.
pub fn lock_workspace() -> Vec<fs::File> {
    use std::os::fd::AsRawFd;
    let cx = ctx();
    fs::create_dir_all(&cx.state)
        .unwrap_or_else(|e| die(&format!("cannot create state directory: {e}")));
    let mut paths = vec![cx.state.join("lock")];
    if cx.source.join(".git").is_dir() {
        paths.insert(0, cx.source.join(".git/kit.lock"));
    }
    paths
        .into_iter()
        .map(|p| {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&p)
                .unwrap_or_else(|e| die(&format!("cannot open lock {}: {e}", p.display())));
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                die("another kit command is running; wait for it to finish");
            }
            f
        })
        .collect()
}

/// Drop a reserved backup folder that ended up empty.
pub fn release_stamp(stamp: &str) {
    let _ = std::fs::remove_dir(ctx().backups.join(stamp));
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if !ok {
        return "this machine".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod query_tests {
    use super::*;
    #[test]
    fn provider_queries_timeout_and_stop_children_holding_pipes() {
        let start = std::time::Instant::now();
        let r = query(&["sh", "-c", "sleep 10"], 1);
        assert_eq!(r.code, 124);
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        let start = std::time::Instant::now();
        let r = query(&["sh", "-c", "sleep 10 & printf done"], 2);
        assert_eq!(r.code, 0);
        assert_eq!(r.stdout, "done");
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
