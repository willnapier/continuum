//! The no-save request, `~/.continuum-nosave`, as the capture wrappers take it.
//!
//! `nosave` creates the marker; the next assistant started through a wrapper
//! is not saved to Continuum, and the marker is used up. "Used up" has to
//! mean that the request reached a session. Before this module the wrappers
//! deleted the marker and then tried to start the harness, so a start that
//! failed (above all a refusal by the mask launcher, `ai-masked`, exit 113)
//! used the request up with nothing started, and the retry was saved.
//!
//! So the request is **reserved, then settled**:
//!
//! 1. [`NoSaveRequest::reserve`] renames the marker to
//!    `.continuum-nosave.reserved-<pid>`, a name only this wrapper process
//!    uses. Rename is atomic, so of two wrappers started together exactly one
//!    takes the request, as before.
//! 2. When the harness has run, [`NoSaveRequest::delivered`] removes the
//!    reservation: the request is used up.
//! 3. When nothing was started, the request is given back: the reservation is
//!    renamed to the marker name again. This is also what happens when the
//!    value is dropped without being settled, so an error on the way to the
//!    start gives the request back too.
//!
//! Every doubtful case errs towards not saving. A harness that itself exits
//! with one of the launcher's "nothing started" codes after a real session
//! gets its request back, and the next session is then not saved either
//! (`masked_launch::launcher_started_nothing`). A wrapper that is killed
//! leaves its reservation file behind; nothing reads it, and no other wrapper
//! touches a reservation that is not its own.

use std::path::{Path, PathBuf};

/// The marker's name in the home folder.
pub const MARKER_NAME: &str = ".continuum-nosave";

/// One no-save request, held by this wrapper process until it is settled.
#[derive(Debug)]
pub struct NoSaveRequest {
    marker: PathBuf,
    /// None when the marker could not be moved and stays where it is.
    reserved: Option<PathBuf>,
}

impl NoSaveRequest {
    /// Take the request for this process, if there is one.
    pub fn reserve(home: &Path) -> Option<Self> {
        Self::reserve_for(home, std::process::id())
    }

    /// [`Self::reserve`] for a stated process id.
    pub fn reserve_for(home: &Path, pid: u32) -> Option<Self> {
        let marker = home.join(MARKER_NAME);
        let reserved = home.join(format!("{MARKER_NAME}.reserved-{pid}"));
        match std::fs::rename(&marker, &reserved) {
            Ok(()) => Some(Self {
                marker,
                reserved: Some(reserved),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            // The marker is there but cannot be moved. The request still
            // counts, as it did when the marker was only looked at. It stays
            // in place, so it covers the next start as well.
            Err(_) if marker.exists() => Some(Self {
                marker,
                reserved: None,
            }),
            Err(_) => None,
        }
    }

    /// The harness ran with the request: use it up.
    pub fn delivered(mut self) {
        if let Some(reserved) = self.reserved.take() {
            let _ = std::fs::remove_file(reserved);
        }
    }

    /// Nothing was started: make the request available again.
    pub fn give_back(self) {
        drop(self);
    }

    fn restore(&mut self) {
        let Some(reserved) = self.reserved.take() else {
            return;
        };
        // One rename, so there is no moment with no request on disk. If a new
        // marker was made meanwhile the rename replaces it, and one request
        // is available either way.
        if std::fs::rename(&reserved, &self.marker).is_err() && self.marker.exists() {
            // The marker name is taken by something a rename cannot replace:
            // the request is available, so only the reservation goes.
            let _ = std::fs::remove_file(&reserved);
        }
    }
}

impl Drop for NoSaveRequest {
    /// Not settled as delivered: give the request back.
    fn drop(&mut self) {
        self.restore();
    }
}

/// Settle a wrapper's request once its child has finished: used up when the
/// harness started, given back when it did not. Returns whether it was given
/// back. Call it before `std::process::exit`, which runs no destructors.
pub fn settle(request: Option<NoSaveRequest>, harness_started: bool) -> bool {
    match request {
        Some(request) if harness_started => {
            request.delivered();
            false
        }
        Some(request) => {
            request.give_back();
            true
        }
        None => false,
    }
}

/// [`settle`] for a wrapper whose child has just finished with `exit_code`.
/// Whether the harness started is read from the code
/// (`masked_launch::launcher_started_nothing`): on Linux the mask launcher's
/// refusal (113) and its failed exec (126, 127) mean nothing was started, and
/// the request is given back. Returns whether the harness started. A wrapper
/// that gets `false` has no session of its own to import, delete or log.
pub fn settle_at_exit(request: Option<NoSaveRequest>, exit_code: Option<i32>) -> bool {
    let started = !crate::masked_launch::launcher_started_nothing(exit_code);
    if settle(request, started) {
        eprintln!("↩ Nothing was started, so the no-save request is kept for the next start");
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn marker(home: &Path) -> PathBuf {
        home.join(MARKER_NAME)
    }
    fn reservation(home: &Path, pid: u32) -> PathBuf {
        home.join(format!("{MARKER_NAME}.reserved-{pid}"))
    }
    /// Everything in the home folder, sorted: the whole state on disk.
    fn files(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn no_marker_is_no_request_and_nothing_is_created() {
        let home = tempdir().unwrap();
        assert!(NoSaveRequest::reserve_for(home.path(), 4242).is_none());
        assert_eq!(files(home.path()), Vec::<String>::new());
    }

    #[test]
    fn a_reserved_request_is_out_of_the_marker_name_until_it_is_settled() {
        let home = tempdir().unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();
        let request = NoSaveRequest::reserve_for(home.path(), 4242).expect("the request is taken");
        assert_eq!(files(home.path()), [".continuum-nosave.reserved-4242"]);
        request.delivered();
        assert_eq!(files(home.path()), Vec::<String>::new(), "delivered: used up");
    }

    #[test]
    fn a_request_that_reached_nothing_is_given_back() {
        let home = tempdir().unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();

        let request = NoSaveRequest::reserve_for(home.path(), 4242).unwrap();
        request.give_back();
        assert_eq!(files(home.path()), [MARKER_NAME], "given back by name");

        // Dropped without being settled, as on an early error return.
        let request = NoSaveRequest::reserve_for(home.path(), 4242).unwrap();
        drop(request);
        assert_eq!(files(home.path()), [MARKER_NAME], "given back on drop");

        assert!(settle(NoSaveRequest::reserve_for(home.path(), 4242), false));
        assert_eq!(files(home.path()), [MARKER_NAME], "given back by settle");
        assert!(!settle(NoSaveRequest::reserve_for(home.path(), 4242), true));
        assert_eq!(files(home.path()), Vec::<String>::new(), "used up by settle");
        assert!(!settle(None, false), "no request, nothing given back");
    }

    #[test]
    fn of_two_wrappers_exactly_one_takes_the_request() {
        let home = tempdir().unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();
        let first = NoSaveRequest::reserve_for(home.path(), 4242);
        let second = NoSaveRequest::reserve_for(home.path(), 4343);
        assert!(first.is_some() && second.is_none());
        assert_eq!(files(home.path()), [".continuum-nosave.reserved-4242"]);
    }

    #[test]
    fn a_new_marker_made_meanwhile_leaves_one_request_after_giving_back() {
        let home = tempdir().unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();
        let request = NoSaveRequest::reserve_for(home.path(), 4242).unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();
        request.give_back();
        assert_eq!(files(home.path()), [MARKER_NAME]);
    }

    #[test]
    fn another_wrappers_reservation_is_never_touched() {
        let home = tempdir().unwrap();
        let foreign = reservation(home.path(), 4343);
        std::fs::write(&foreign, b"held by 4343").unwrap();

        // It is not a request for this wrapper.
        assert!(NoSaveRequest::reserve_for(home.path(), 4242).is_none());

        // And it survives this wrapper using up, and giving back, its own.
        for harness_started in [true, false] {
            std::fs::write(marker(home.path()), b"").unwrap();
            settle(NoSaveRequest::reserve_for(home.path(), 4242), harness_started);
            assert_eq!(std::fs::read(&foreign).unwrap(), b"held by 4343");
            let _ = std::fs::remove_file(marker(home.path()));
        }
        assert_eq!(files(home.path()), [".continuum-nosave.reserved-4343"]);
    }

    #[test]
    fn a_marker_that_cannot_be_moved_still_counts_and_stays() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempdir().unwrap();
        std::fs::write(marker(home.path()), b"").unwrap();
        let read_only = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(home.path(), read_only).unwrap();
        let request = NoSaveRequest::reserve_for(home.path(), 4242);
        let held = request.is_some();
        if let Some(request) = request {
            request.delivered();
        }
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(held, "the request is honoured although the marker could not be moved");
        assert_eq!(files(home.path()), [MARKER_NAME], "and the marker is still there");
    }
}
