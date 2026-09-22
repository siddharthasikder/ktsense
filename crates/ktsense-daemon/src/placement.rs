//! Where a root's daemon socket goes when the obvious place is too deep to bind.
//!
//! A Unix domain address is a fixed-size `sun_path`, not a pathname of arbitrary length, so socket
//! placement is a budget problem and not only a naming one. macOS makes that visible where Linux
//! does not: its per-user `TMPDIR` is around fifty bytes deep before anything is nested under it, so
//! a test that derives `XDG_RUNTIME_DIR` from a temporary directory there reached a 101-byte socket
//! path whose eight-byte `.start0` claim could not be bound at all, and the start failed with `path
//! must be shorter than SUN_LEN` (KT-66, observed on macos-14 CI). The claim is one shorter file now
//! rather than one of sixty-four generations (KT-71), and the reserve still covers the longest of
//! them so a budget widened here cannot reintroduce the failure.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::server::current_uid;
use crate::{path_key, socket_dir, socket_path};

/// Longest socket path this crate will place anywhere. `sockaddr_un.sun_path` holds 104 bytes on
/// Darwin and 108 on Linux, one of which the terminating NUL takes, so 103 is what the tightest
/// supported platform accepts.
///
/// The tightest limit applies on every platform deliberately. A budget that widened on Linux would
/// make a macOS overflow unreproducible on the machines this is developed on, which is how the
/// KT-66 failure reached CI unnoticed in the first place.
pub const MAX_SOCKET_PATH: usize = 103;

/// Bytes kept free after a socket path for the companion files a start puts beside it. The one it
/// places today is `daemon start`'s claim, `<socket>.start`, at six bytes; the reserve stays at eight
/// because that covered `<socket>.start63` when a claim had generations (KT-71 removed them) and
/// narrowing a budget buys nothing. A socket path that fits while its own claim does not is the exact
/// shape of the KT-66 failure, so the budget covers both rather than the socket alone.
pub const COMPANION_RESERVE: usize = 8;

/// Longest socket path a placement will choose, companions included.
pub const SOCKET_BUDGET: usize = MAX_SOCKET_PATH - COMPANION_RESERVE;

/// Why no socket path could be placed for a root.
///
/// Both variants end the command that asked. A daemon that bound a path it could not address, or one
/// reachable by another user, is worse than a refusal that names the reason.
#[derive(Debug, Error)]
pub enum SocketPlacementError {
    #[error(
        "no daemon socket path fits in {SOCKET_BUDGET} bytes: {preferred} needs {preferred_len} \
         and the short fallback {fallback} needs {fallback_len}"
    )]
    NoShortPath {
        preferred: PathBuf,
        preferred_len: usize,
        fallback: PathBuf,
        fallback_len: usize,
    },
    #[error("{base} cannot hold a daemon socket: {fault}")]
    UnsafeShortBase { base: PathBuf, fault: BaseFault },
    #[error("cannot inspect {base}: {source}")]
    UnreadableShortBase {
        base: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What disqualifies a short base from holding a socket. Each is a way the directory could have been
/// prepared by somebody else, which matters because the short base lives where every user can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BaseFault {
    #[error("it is owned by uid {0} rather than by this user")]
    ForeignOwner(u32),
    #[error("mode {0:04o} lets another user write into it")]
    WritableByOthers(u32),
    #[error("it is a symbolic link, which could point anywhere")]
    Symlink,
    #[error("it is not a directory")]
    NotADirectory,
}

/// Resolves where `workspace_root`'s daemon socket goes. The precedence, in order:
///
/// 1. `$XDG_RUNTIME_DIR/ktsense`, or `$HOME/.cache/ktsense/run` when the runtime dir is unset, which
///    is what [`socket_dir`] chooses and what every platform with a short runtime directory gets.
/// 2. `<short_base>/<key of the directory step 1 asked for>`, when step 1's socket path would not fit
///    [`SOCKET_BUDGET`]. Keying the fallback by the rejected directory rather than collapsing every
///    caller onto one place is load-bearing: two callers given different runtime directories keep
///    different sockets, which is what per-login runtime directories and test isolation both rest on.
///
/// Callers must resolve through here rather than reimplementing the choice, since the CLI parent, the
/// detached `daemon serve` child and every routed command have to agree on one path.
pub fn resolve_socket_path(
    xdg_runtime_dir: Option<&Path>,
    home: &Path,
    short_base: &Path,
    workspace_root: &Path,
) -> Result<PathBuf, SocketPlacementError> {
    let preferred_dir = socket_dir(xdg_runtime_dir, home);
    let preferred = socket_path(&preferred_dir, workspace_root);
    if within_budget(&preferred) {
        return Ok(preferred);
    }
    let fallback = socket_path(&short_base.join(path_key(&preferred_dir)), workspace_root);
    if !within_budget(&fallback) {
        return Err(SocketPlacementError::NoShortPath {
            preferred_len: length(&preferred),
            preferred,
            fallback_len: length(&fallback),
            fallback,
        });
    }
    accept_short_base(short_base)?;
    Ok(fallback)
}

/// The short place a fallback socket goes: a per-uid directory directly under `/tmp`.
///
/// `/tmp` rather than `TMPDIR`, because on macOS `TMPDIR` is the deep per-user directory being
/// escaped. The uid is in the name so two users never contend for one directory, and
/// [`resolve_socket_path`] refuses a base that is not this user's own, because `/tmp` is writable by
/// everyone and a squatter could otherwise present the directory a socket lands in.
pub fn short_socket_base() -> PathBuf {
    PathBuf::from(format!("/tmp/ktsense-{}", current_uid()))
}

fn within_budget(path: &Path) -> bool {
    length(path) <= SOCKET_BUDGET
}

/// Bytes, not characters: `sun_path` is a byte buffer, so a multi-byte path costs what it weighs.
fn length(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}

/// Reads the short base, refusing one somebody else prepared. A base that does not exist yet is
/// accepted: whoever needs it creates it owner-only, and creating it here would make resolving a
/// path a change to the filesystem.
fn accept_short_base(base: &Path) -> Result<(), SocketPlacementError> {
    match std::fs::symlink_metadata(base) {
        Ok(metadata) => match fault_in(&Base::from(&metadata), current_uid()) {
            Some(fault) => Err(SocketPlacementError::UnsafeShortBase {
                base: base.to_path_buf(),
                fault,
            }),
            None => Ok(()),
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SocketPlacementError::UnreadableShortBase {
            base: base.to_path_buf(),
            source,
        }),
    }
}

/// What the short base is, as the facts read off its metadata, so the decision below can be pinned
/// without a second user or a root shell to create the cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Base {
    symlink: bool,
    directory: bool,
    uid: u32,
    mode: u32,
}

impl From<&std::fs::Metadata> for Base {
    fn from(metadata: &std::fs::Metadata) -> Self {
        Self {
            symlink: metadata.file_type().is_symlink(),
            directory: metadata.is_dir(),
            uid: metadata.uid(),
            mode: metadata.permissions().mode(),
        }
    }
}

const WRITABLE_BY_OTHERS: u32 = 0o022;

fn fault_in(base: &Base, current: u32) -> Option<BaseFault> {
    if base.symlink {
        Some(BaseFault::Symlink)
    } else if !base.directory {
        Some(BaseFault::NotADirectory)
    } else if base.uid != current {
        Some(BaseFault::ForeignOwner(base.uid))
    } else if base.mode & WRITABLE_BY_OTHERS != 0 {
        Some(BaseFault::WritableByOthers(base.mode & 0o7777))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_BASE: &str = "/tmp/ktsense-1000";
    const ROOT: &str = "/work/alpha";

    /// A runtime directory of `depth` bytes, shaped like the one macOS hands a test: an absolute path
    /// whose segments are meaningless and whose length is the whole point.
    fn runtime_dir(depth: usize) -> PathBuf {
        let mut path = String::from("/d");
        while path.len() < depth {
            path.push(if path.len() % 8 == 0 { '/' } else { 'x' });
        }
        PathBuf::from(path)
    }

    /// The documented precedence, read off one pair of resolutions: a runtime directory short enough
    /// keeps its socket, and one too deep is answered from the short base instead of being refused or
    /// silently placed where it cannot be bound. The deep case is proven to have forced the overflow
    /// rather than assumed it, since a budget change that stopped rejecting it would otherwise turn
    /// this into a test of nothing.
    #[test]
    fn a_runtime_dir_keeps_its_socket_until_the_path_no_longer_fits() {
        let shallow = runtime_dir(20);
        let deep = runtime_dir(SOCKET_BUDGET);
        let place = |runtime: &Path| {
            resolve_socket_path(
                Some(runtime),
                Path::new("/home/dev"),
                Path::new(SHORT_BASE),
                Path::new(ROOT),
            )
            .expect("a placement")
        };

        let kept = place(&shallow);
        let moved = place(&deep);
        let rejected = socket_path(
            &socket_dir(Some(&deep), Path::new("/home/dev")),
            Path::new(ROOT),
        );

        assert_eq!(
            (
                kept.starts_with(&shallow),
                length(&kept) <= SOCKET_BUDGET,
                length(&rejected) > SOCKET_BUDGET,
                moved.starts_with(SHORT_BASE),
                length(&moved) <= SOCKET_BUDGET,
            ),
            (true, true, true, true, true)
        );
    }

    /// The fallback keeps the identity of the directory it replaced. Two callers handed different
    /// runtime directories must not be answered with one socket: that would hand a test another
    /// test's daemon, and a second login another login's.
    #[test]
    fn two_deep_runtime_dirs_fall_back_to_two_different_sockets() {
        let place = |runtime: &Path, root: &str| {
            resolve_socket_path(
                Some(runtime),
                Path::new("/home/dev"),
                Path::new(SHORT_BASE),
                Path::new(root),
            )
            .expect("a placement")
        };
        let first = runtime_dir(SOCKET_BUDGET);
        let second = runtime_dir(SOCKET_BUDGET + 1);

        assert_eq!(
            (
                place(&first, ROOT) == place(&second, ROOT),
                place(&first, ROOT) == place(&first, ROOT),
                place(&first, ROOT) == place(&first, "/work/beta"),
            ),
            (false, true, false)
        );
    }

    /// When even the short base cannot yield a bindable path there is nothing honest to do but say
    /// so, naming both candidates and what they needed. Placing the path anyway would move the
    /// failure to a `bind` the caller cannot explain, which is the KT-66 symptom.
    #[test]
    fn a_short_base_that_is_itself_too_deep_is_reported_rather_than_placed() {
        let placed = resolve_socket_path(
            Some(&runtime_dir(SOCKET_BUDGET)),
            Path::new("/home/dev"),
            &runtime_dir(SOCKET_BUDGET),
            Path::new(ROOT),
        );

        assert!(
            matches!(
                placed,
                Err(SocketPlacementError::NoShortPath {
                    preferred_len,
                    fallback_len,
                    ..
                }) if preferred_len > SOCKET_BUDGET && fallback_len > SOCKET_BUDGET
            ),
            "expected an honest refusal, got {placed:?}"
        );
    }

    /// The short base sits where every user can write, so a base somebody else prepared is refused
    /// rather than used. Each refusal is a way that could happen; ownership by this user with no
    /// write bit for anyone else is the only accepted shape.
    #[test]
    fn only_a_private_directory_owned_by_this_user_may_hold_a_fallback_socket() {
        let mine = 1000;
        let read = |base: Base| fault_in(&base, mine);
        let directory = |uid: u32, mode: u32| Base {
            symlink: false,
            directory: true,
            uid,
            mode,
        };

        assert_eq!(
            (
                read(directory(mine, 0o040700)),
                read(directory(mine + 1, 0o040700)),
                read(directory(mine, 0o040707)),
                read(directory(mine, 0o040770)),
                read(Base {
                    symlink: true,
                    directory: true,
                    uid: mine,
                    mode: 0o040700
                }),
                read(Base {
                    symlink: false,
                    directory: false,
                    uid: mine,
                    mode: 0o100600
                }),
            ),
            (
                None,
                Some(BaseFault::ForeignOwner(mine + 1)),
                Some(BaseFault::WritableByOthers(0o707)),
                Some(BaseFault::WritableByOthers(0o770)),
                Some(BaseFault::Symlink),
                Some(BaseFault::NotADirectory),
            )
        );
    }

    /// The refusal is made against the real filesystem too, not only against hand-built metadata: a
    /// world-writable base is rejected, and a base that does not exist yet is accepted because
    /// whoever needs it creates it owner-only.
    #[test]
    fn a_world_writable_base_on_disk_is_refused_and_a_missing_one_is_accepted() {
        let home = tempfile::tempdir().expect("temp dir");
        let open = home.path().join("open");
        std::fs::create_dir(&open).expect("create the base");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777))
            .expect("open it up");

        assert_eq!(
            (
                matches!(
                    accept_short_base(&open),
                    Err(SocketPlacementError::UnsafeShortBase {
                        fault: BaseFault::WritableByOthers(_),
                        ..
                    })
                ),
                accept_short_base(&home.path().join("absent")).is_ok(),
            ),
            (true, true)
        );
    }
}
