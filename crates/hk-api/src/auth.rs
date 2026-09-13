//! Bearer token for the HTTP API and the WebSocket bridge (docs/stream-contract.md §11: remote
//! listeners must authenticate), and for the control API (T-050).
//!
//! - One token per server, generated from the OS CSPRNG (`/dev/urandom`, 256 bits, hex), supplied
//!   by configuration (at least [`MIN_TOKEN_LEN`] characters), or kept in a **token file**
//!   ([`Token::load_or_create`]): created on first run with mode `0600` in a `0700` directory
//!   ([`default_token_path`]: `$HK_TOKEN_FILE`, else `$XDG_CONFIG_HOME/hackriff/api-token`, else
//!   `~/.config/hackriff/api-token`), refused when group- or world-accessible or not a regular
//!   file. The same token then survives restarts, so a browser (or the cloudflared tunnel URL)
//!   keeps working.
//! - Compared in constant time over the full length ([`Token::verify`]); an **expired** token
//!   ([`Token::with_expiry`]) never verifies.
//! - Clients send it as `Authorization: Bearer <token>`. Only read-only requests may use the
//!   `token` query parameter instead (the browser `WebSocket` API cannot set headers); control
//!   requests must use the header, so the token never lands in URLs, proxy logs or history for
//!   a mutating call.
//! - [`Token::id`] is a non-secret fingerprint (`tok-` + 12 hex digits of SHA-256) for audit logs.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hk_model::ContentHash;

/// Shortest configured token accepted.
pub const MIN_TOKEN_LEN: usize = 16;

/// An API token.
#[derive(Clone)]
pub struct Token {
    value: String,
    expires: Option<SystemTime>,
}

impl Token {
    /// A fresh 256-bit token, hex encoded.
    pub fn generate() -> io::Result<Self> {
        let mut raw = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut raw)?;
        Ok(Self {
            value: raw.iter().map(|b| format!("{b:02x}")).collect(),
            expires: None,
        })
    }

    /// A configured token: [`MIN_TOKEN_LEN`]..=256 printable ASCII characters, no spaces.
    pub fn from_config(value: &str) -> Result<Self, String> {
        let ok_chars = value.bytes().all(|b| b.is_ascii_graphic());
        if !(MIN_TOKEN_LEN..=256).contains(&value.len()) || !ok_chars {
            return Err(format!(
                "token must be {MIN_TOKEN_LEN}..=256 printable ASCII characters without spaces"
            ));
        }
        Ok(Self {
            value: value.to_owned(),
            expires: None,
        })
    }

    /// Reads the token file at `path`, or creates it (mode `0600`, parent directories `0700`)
    /// holding a generated token. Returns the token and whether the file was created. Refuses a
    /// file that is not a regular file (a symlink included), is accessible by group or others,
    /// or holds an invalid token.
    pub fn load_or_create(path: &Path) -> io::Result<(Self, bool)> {
        match Self::load(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => return other.map(|t| (t, false)),
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.exists() {
                fs::create_dir_all(dir)?;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        check_parent(path)?;
        let token = Self::generate()?;
        // `create_new` (O_EXCL) never follows a symlink; O_NOFOLLOW states it explicitly.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(mut f) => {
                f.write_all(format!("{}\n", token.value).as_bytes())?;
                f.sync_all()?;
                Ok((token, true))
            }
            // Another process created it first: use theirs.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Self::load(path).map(|t| (t, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Opens without following a symlink (and without blocking on a FIFO), then checks the
    /// opened descriptor: a regular file, owned by the effective user, not accessible by group or
    /// others; and the parent directory is not group- or world-writable.
    fn load(path: &Path) -> io::Result<Self> {
        check_parent(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("token file {} is a symlink", path.display()),
                    )
                } else {
                    e
                }
            })?;
        let meta = file.metadata()?;
        check_file(
            path,
            meta.file_type().is_file(),
            meta.uid(),
            meta.mode(),
            euid(),
        )?;
        let mut text = String::new();
        file.take(4096).read_to_string(&mut text)?;
        Self::from_config(text.trim()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("token file {}: {e}", path.display()),
            )
        })
    }

    /// This token, no longer valid from `at` on.
    pub fn with_expiry(mut self, at: SystemTime) -> Self {
        self.expires = Some(at);
        self
    }

    /// When the token stops being valid, if ever.
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires
    }

    /// The token has expired.
    pub fn is_expired(&self) -> bool {
        self.expires.is_some_and(|t| SystemTime::now() >= t)
    }

    /// The token text (to print the connect URL once at start).
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// A non-secret fingerprint for audit logs: `tok-` and the first 12 hex digits of the
    /// token's SHA-256.
    pub fn id(&self) -> String {
        format!("tok-{}", &ContentHash::of_text(&self.value).to_hex()[..12])
    }

    /// Whether `candidate` equals the token and the token has not expired. Runs in time that
    /// depends only on the lengths: every byte is compared, and a length mismatch still walks
    /// the full token.
    pub fn verify(&self, candidate: &str) -> bool {
        let (a, b) = (self.value.as_bytes(), candidate.as_bytes());
        let mut diff = (a.len() ^ b.len()) as u64;
        for (i, &x) in a.iter().enumerate() {
            let y = b.get(i).copied().unwrap_or(0);
            diff |= u64::from(x ^ y);
        }
        diff == 0 && !self.is_expired()
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The token file's own checks, on the opened descriptor's metadata.
fn check_file(path: &Path, regular: bool, uid: u32, mode: u32, euid: u32) -> io::Result<()> {
    if !regular {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("token file {} is not a regular file", path.display()),
        ));
    }
    if uid != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "token file {} is owned by uid {uid}, not the current user (uid {euid})",
                path.display()
            ),
        ));
    }
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "token file {} is accessible by group or others (mode {:o}); run chmod 600 on it",
                path.display(),
                mode & 0o777
            ),
        ));
    }
    Ok(())
}

/// Refuses a token file whose directory is group- or world-writable (someone else could swap
/// the file). A missing directory passes (the open then reports `NotFound`).
fn check_parent(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    match fs::metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.mode() & 0o022 != 0 => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "token directory {} is writable by group or others (mode {:o}); run chmod 700 on it",
                dir.display(),
                m.mode() & 0o777
            ),
        )),
        Ok(_) => Ok(()),
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Where the API token file lives: `$HK_TOKEN_FILE`, else `$XDG_CONFIG_HOME/hackriff/api-token`,
/// else `$HOME/.config/hackriff/api-token`; `None` without any of them.
pub fn default_token_path() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(p) = non_empty("HK_TOKEN_FILE") {
        return Some(PathBuf::from(p));
    }
    let config = non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("hackriff").join("api-token"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert_ne!(a.expose(), b.expose());
        assert!(a.verify(a.expose()));
        assert!(!a.verify(b.expose()));
        assert_ne!(a.id(), b.id());
        assert!(a.id().starts_with("tok-") && a.id().len() == 16);
        assert!(
            !a.id().contains(&a.expose()[..12]),
            "the id is not a prefix"
        );
    }

    #[test]
    fn verify_rejects_prefixes_extensions_and_empty() {
        let t = Token::from_config("0123456789abcdef").unwrap();
        assert!(t.verify("0123456789abcdef"));
        assert!(!t.verify("0123456789abcde"));
        assert!(!t.verify("0123456789abcdef0"));
        assert!(!t.verify(""));
        assert!(!t.verify("0123456789abcdeF"));
        assert!(Token::from_config("short").is_err());
        assert!(Token::from_config("has a space in it!").is_err());
        assert_eq!(format!("{t:?}"), "Token(<redacted>)");
    }

    #[test]
    fn expired_tokens_never_verify() {
        let t = Token::from_config("0123456789abcdef").unwrap();
        let past = t
            .clone()
            .with_expiry(SystemTime::now() - Duration::from_secs(1));
        assert!(past.is_expired());
        assert!(!past.verify("0123456789abcdef"));
        let future = t.with_expiry(SystemTime::now() + Duration::from_secs(3600));
        assert!(future.verify("0123456789abcdef"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hk-api-token-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_token_file_is_created_private_and_reused() {
        let dir = scratch("file");
        let path = dir.join("cfg").join("api-token");
        let (a, created) = Token::load_or_create(&path).unwrap();
        assert!(created);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file mode");
        let dmode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "token directory mode");
        let (b, created) = Token::load_or_create(&path).unwrap();
        assert!(!created);
        assert_eq!(a.expose(), b.expose(), "the same token after a restart");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let e = Token::load_or_create(&path).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, "short\n").unwrap();
        assert_eq!(
            Token::load_or_create(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let link = dir.join("cfg").join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            Token::load_or_create(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // A dangling symlink is not followed to create a file elsewhere.
        let dangling = dir.join("cfg").join("dangling");
        let target = dir.join("elsewhere");
        std::os::unix::fs::symlink(&target, &dangling).unwrap();
        assert!(Token::load_or_create(&dangling).is_err());
        assert!(!target.exists(), "nothing was created through the symlink");
        // A FIFO is refused without blocking.
        let fifo = dir.join("cfg").join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(
            Token::load_or_create(&fifo).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_writable_token_directory_or_foreign_owner_is_refused() {
        let dir = scratch("dir");
        let path = dir.join("cfg").join("api-token");
        Token::load_or_create(&path).unwrap();
        let cfg = path.parent().unwrap();
        for mode in [0o770, 0o707, 0o722] {
            fs::set_permissions(cfg, fs::Permissions::from_mode(mode)).unwrap();
            let e = Token::load_or_create(&path).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{mode:o}: {e}");
        }
        fs::set_permissions(cfg, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            Token::load_or_create(&path).is_ok(),
            "read-only group access is fine"
        );
        // Owner and mode checks (a file owned by another uid cannot be made without root).
        let p = Path::new("t");
        assert!(check_file(p, true, 501, 0o100600, 501).is_ok());
        let e = check_file(p, true, 0, 0o100600, 501).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("owned by uid 0"));
        assert_eq!(
            check_file(p, false, 501, 0o100600, 501).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            check_file(p, true, 501, 0o100640, 501).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = fs::remove_dir_all(dir);
    }
}
