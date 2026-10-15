use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// Drops from root to the user that invoked `sudo`, once every privileged
/// port (`:443`, `:53`) has already been bound. Every process the daemon
/// spawns from this point on — and the daemon itself — inherits the
/// dropped-to uid/gid instead of root.
///
/// Without this, *every* supervised process (Rails, Sidekiq, a Node dev
/// server, all of them, not just the ones that happen to hit the symptom
/// first) runs as root: Postgres peer-auth over a unix socket then
/// authenticates as role `root` instead of the real user, and anything the
/// process creates (`node_modules`, build output, logs) ends up root-owned
/// on disk. This does *not* fix child processes missing `~/.zshrc`-only env
/// vars — `sh -c "<command>"` never sources `.zshrc` regardless of uid,
/// since it's not an interactive shell; that's a separate problem, solved
/// by putting the var in `alcazar.toml`'s `[env]` instead.
///
/// No-op if we're not actually running as root (e.g. running in dev without
/// `sudo`, or a config with no routes/dns needing a privileged bind).
///
/// Two different ways to end up here as root, and each says who to drop to
/// differently:
/// - via `sudo alcazar start`: effective uid is 0, real uid is also 0 (sudo
///   doesn't preserve it), so the *only* way to recover the original user is
///   `SUDO_UID`/`SUDO_GID`/`SUDO_USER`, which sudo sets in the environment.
/// - via a setuid-root binary (`chmod u+s`) invoked directly, no `sudo`:
///   effective uid is 0 (forced by the setuid bit), but the real uid
///   (`getuid()`) is still the invoking user's own — that's the classic way
///   a setuid daemon finds out who actually ran it.
pub fn drop_to_invoking_user() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }

    let (uid, gid, username) = if let Some(uid) = env_u32("SUDO_UID") {
        let gid = env_u32("SUDO_GID").context("SUDO_UID is set but SUDO_GID is missing")?;
        let username =
            std::env::var("SUDO_USER").context("SUDO_UID is set but SUDO_USER is missing")?;
        (uid, gid, username)
    } else {
        let ruid = unsafe { libc::getuid() };
        if ruid == 0 {
            warn!(
                "running as root with no SUDO_UID in the environment and no setuid-invoking \
                 user either — staying root for the rest of this run. Start alcazar with \
                 `sudo alcazar start`, or make the binary setuid-root, so privileges can be \
                 dropped after binding :443/:53."
            );
            return Ok(());
        }
        let rgid = unsafe { libc::getgid() };
        let username = username_for_uid(ruid)?;
        (ruid, rgid, username)
    };
    let c_username = std::ffi::CString::new(username.clone())
        .context("invoking username contains a NUL byte")?;

    // The state dir (logs, pid files, the IPC socket) was created root-owned
    // by every run before this one — mode 755, so the invoking user's group
    // membership alone doesn't grant write access. Hand it back before we
    // drop, or every process spawned from here on fails to open its own log
    // file the moment it tries.
    let state_dir = crate::config::dirs_home().join(".alcazar");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    chown_recursive(&state_dir, uid, gid)
        .with_context(|| format!("chowning {} to uid {uid}", state_dir.display()))?;

    // initgroups needs root (it reads /etc/group) so it must run before we
    // give that up; setgid must run before setuid — once we're not root
    // anymore we no longer have permission to change the gid.
    if unsafe { libc::initgroups(c_username.as_ptr(), gid as libc::c_int) } != 0 {
        return Err(std::io::Error::last_os_error()).context("initgroups failed");
    }
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setgid failed");
    }
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setuid failed");
    }

    // setuid(0) succeeding here would mean the drop above didn't actually
    // stick (e.g. a leftover saved-uid still root) — better to fail loudly
    // than to keep running while believing we're unprivileged.
    if unsafe { libc::setuid(0) } == 0 {
        bail!("dropped privileges but could still regain root — aborting");
    }

    info!(user = %username, uid, gid, "dropped root privileges after binding privileged ports");
    Ok(())
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.parse().ok()
}

/// Looks up the username for a uid via the reentrant `getpwuid_r` (not the
/// classic `getpwuid`, which returns a pointer into static storage that
/// isn't safe to touch from a process that already has other threads
/// running — true here, since this runs inside a `tokio` multi-thread
/// runtime). Only used for the setuid-invocation path: `sudo` gives us
/// `SUDO_USER` directly, no lookup needed.
fn username_for_uid(uid: u32) -> Result<String> {
    let mut buf = vec![0i8; 16384];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();

    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc)).context("getpwuid_r failed");
    }
    if result.is_null() {
        bail!("no passwd entry for uid {uid}");
    }
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Ok(name.to_string_lossy().into_owned())
}

fn chown_recursive(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_recursive(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}
