//! Crash-safe writes and cross-process exclusion.
//!
//! Every file this program persists — the configuration, the fallback
//! credentials, and the control directory's records — is replaced whole: the
//! new bytes go to a temporary file in the same directory, are flushed to the
//! medium, and are then `rename`d over the target. A concurrent reader sees
//! either the old file or the new one, never a truncated mixture, and an
//! interrupted write leaves the old file untouched.
//!
//! Writers that must not lose a concurrent update (the configuration's
//! read-modify-write, a credential transaction) hold an **exclusive lock on a
//! sidecar file** named `<target>.lock` for the whole operation, including the
//! re-read. The lock file is created once and never deleted: an unlink would
//! let a second process lock a fresh inode while the first still holds the old
//! one. The target itself is never locked, because it is replaced by `rename`
//! and a lock on the replaced inode would protect nothing.
//!
//! Secrecy is a property of creation, not of a later `chmod`: a private file is
//! created `0600` (on Windows, with a DACL that admits only this user and
//! `SYSTEM`, applied before any secret byte is written), and a private new
//! directory is created `0700`. Private and control files must not be symlinks
//! or other special files — writing through a link would put the secret where
//! the link points. An ordinary configuration file that is already a symlink
//! keeps its link: the write resolves to the link's target.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The write target for `path`, following an existing symlink to its target.
///
/// A configuration file managed by a dotfiles repository is often a symlink;
/// replacing the link itself would silently detach it. Resolving keeps the link
/// pointing at the file whose bytes change.
pub fn resolve_write_target(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(err) => Err(err),
    }
}

/// Refuse to write a secret through anything but a regular file.
///
/// Called for private targets only: a symlink there is a way to redirect the
/// secret, and a directory or device is never what the caller meant.
pub fn reject_private_special(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link; refusing to write a secret through it",
                path.display()
            ),
        )),
        Ok(meta) if !meta.file_type().is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// The sidecar lock file belonging to `target`.
pub fn lock_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    target.with_file_name(name)
}

/// Whether the lock for `target` is currently held by someone.
///
/// Read-only on purpose: a command that only reports state must not create the
/// lock file, the directory it lives in, or anything else. A missing lock file
/// means nobody holds it.
pub fn is_locked(target: &Path) -> io::Result<bool> {
    let lock = lock_path(target);
    match std::fs::symlink_metadata(&lock) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    }
    let file = OpenOptions::new().read(true).write(true).open(&lock)?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

/// An exclusive cross-process lock held for as long as this value lives.
///
/// The lock covers the caller's whole read-modify-write, not merely the write:
/// re-reading inside it is what makes two concurrent writers additive instead
/// of last-write-wins. The guard is not re-entrant — taking the same lock twice
/// on one thread blocks forever — so an operation that holds it calls
/// [`atomic_write`], never [`FileLock::acquire`] again.
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Block until the lock for `target` is ours.
    pub fn acquire(target: &Path) -> io::Result<FileLock> {
        let lock = lock_path(target);
        if let Some(parent) = lock.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = open_lock_file(&lock)?;
        file.lock()?;
        Ok(FileLock { file })
    }

    /// Try to take the lock, returning `Ok(None)` when another process holds
    /// it. Used to answer "is a background run in progress?" without waiting.
    pub fn try_acquire(target: &Path) -> io::Result<Option<FileLock>> {
        let lock = lock_path(target);
        if let Some(parent) = lock.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = open_lock_file(&lock)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(FileLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // A failed unlock releases when the descriptor closes anyway.
        let _ = self.file.unlock();
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Atomically replace `path` with `bytes`.
///
/// `private` selects the secrecy contract: `0600` from creation on Unix, a
/// user-and-`SYSTEM` DACL set before the first byte on Windows. The caller may
/// hold the target's sidecar lock; this function does not acquire it.
pub fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let target = if private {
        reject_private_special(path)?;
        path.to_path_buf()
    } else {
        resolve_write_target(path)?
    };
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    ensure_dir(&parent, private)?;

    let mut temp = TempFile::create(&parent, &target, private)?;
    {
        let mut file = temp.file.try_clone()?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(temp.path(), &target)?;
    temp.keep();
    #[cfg(unix)]
    sync_dir(&parent)?;
    Ok(())
}

/// Create `dir` (and its parents) when missing; a private leaf becomes `0700`.
fn ensure_dir(dir: &Path, private: bool) -> io::Result<()> {
    if dir.is_dir() {
        if private {
            tighten_dir(dir)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    if private {
        tighten_dir(dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn tighten_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// A `create_new` temporary file beside the target, removed on drop unless the
/// rename consumed it.
struct TempFile {
    path: PathBuf,
    file: File,
    kept: bool,
}

impl TempFile {
    fn create(dir: &Path, target: &Path, private: bool) -> io::Result<TempFile> {
        let stem = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        for attempt in 0..128u32 {
            let mut name = std::ffi::OsString::from(".");
            name.push(&stem);
            name.push(format!(".tmp-{}-{attempt}", std::process::id()));
            let path = dir.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(if private { 0o600 } else { 0o644 });
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(windows)]
                    if private {
                        if let Err(err) = protect_windows(&path) {
                            drop(file);
                            let _ = std::fs::remove_file(&path);
                            return Err(err);
                        }
                    }
                    return Ok(TempFile {
                        path,
                        file,
                        kept: false,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free temporary name beside {}", target.display()),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn keep(&mut self) {
        self.kept = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    // Best effort: some filesystems refuse `fsync` on a directory, and the
    // rename itself has already happened at this point.
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
    Ok(())
}

/// Give a freshly created private file a DACL that admits only the current
/// user and `SYSTEM`, with inheritance disabled.
///
/// Applied to the temporary file before any secret is written, so the bytes are
/// never exposed under a wider ACL. Any failure is returned: falling back to the
/// inherited ACL would silently publish the secret to whoever else the parent
/// directory admits.
#[cfg(windows)]
fn protect_windows(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut buffer = vec![0u8; needed.max(1) as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        let closed = windows_sys::Win32::Foundation::CloseHandle(token);
        if ok == 0 || closed == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = buffer.as_ptr().cast::<TOKEN_USER>().read_unaligned();
        let mut sid_text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0usize;
        while *sid_text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len));
        LocalFree(sid_text.cast());

        let sddl = wide(&format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)"));
        let mut descriptor = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        let got = GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted);
        if got == 0 || present == 0 {
            let err = io::Error::last_os_error();
            LocalFree(descriptor);
            return Err(err);
        }
        let path_wide = wide(&path.to_string_lossy());
        let status = SetNamedSecurityInfoW(
            path_wide.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        );
        LocalFree(descriptor);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "srun-portal-persistence-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_write_replaces_the_whole_file() {
        let dir = TempDir::new("replace");
        let path = dir.path().join("notes.toml");
        std::fs::write(&path, "old = 1\n").unwrap();

        atomic_write(&path, b"new = 2\n", false).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 2\n");
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_0600_from_creation_and_a_new_private_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("private");
        let nested = dir.path().join("state").join("secrets.toml");

        atomic_write(&nested, b"passwords = {}\n", true).unwrap();

        let file = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(file, 0o600);
        let parent = std::fs::metadata(nested.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_wider_private_file_is_replaced_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("tighten");
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write(&path, b"new\n", true).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_target_that_is_a_symlink_is_refused_untouched() {
        let dir = TempDir::new("symlink-secret");
        let real = dir.path().join("elsewhere.txt");
        std::fs::write(&real, "untouched\n").unwrap();
        let link = dir.path().join("secrets.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = atomic_write(&link, b"secret\n", true).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_configuration_symlink_is_kept_and_its_target_replaced() {
        let dir = TempDir::new("symlink-config");
        let real = dir.path().join("dotfiles.toml");
        std::fs::write(&real, "a = 1\n").unwrap();
        let link = dir.path().join("srun-portal.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write(&link, b"a = 2\n", false).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "a = 2\n");
    }

    #[test]
    fn asking_whether_a_lock_is_held_creates_nothing() {
        let dir = TempDir::new("is-locked");
        let target = dir.path().join("nested").join("settings.toml");

        assert!(!is_locked(&target).unwrap());
        assert!(!dir.path().join("nested").exists(), "nothing was created");

        let guard = FileLock::acquire(&target).unwrap();
        assert!(is_locked(&target).unwrap());
        drop(guard);
        assert!(!is_locked(&target).unwrap());
    }

    #[test]
    fn the_lock_is_a_sidecar_that_survives_unlocking() {
        let dir = TempDir::new("lock");
        let target = dir.path().join("settings.toml");

        {
            let _guard = FileLock::acquire(&target).unwrap();
            assert!(FileLock::try_acquire(&target).unwrap().is_none());
        }
        assert!(lock_path(&target).exists(), "the lock file is not removed");
        assert!(!target.exists(), "locking does not create the target");

        // Released on drop, so the next holder gets it.
        assert!(FileLock::try_acquire(&target).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("lock-mode");
        let target = dir.path().join("settings.toml");
        drop(FileLock::acquire(&target).unwrap());
        let mode = std::fs::metadata(lock_path(&target))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
