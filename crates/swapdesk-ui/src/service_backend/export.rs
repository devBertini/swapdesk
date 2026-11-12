//! `Command::ActivityExport` in the backend: the service lists the activity log (op 21) and this process writes it
//! to the file the person picked, as CSV (`format::activity_csv`). The service never opens a path a client sent
//! (CONTRACTS C2.1), so the file is the interface's own work.
//!
//! The listing and the write take time, so they never run on the backend thread: each export gets a worker thread,
//! whose request goes out through the backend thread and comes back through the reader thread, as an elevation's do.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use swapdesk_ipc::{MAX_ACTIVITY, ReplyBody, UiOp};

use super::{Inner, Item, convert, elevation, lock};
use crate::api::{ApiCode, ApiError, Language, Reply, Ticket};

/// Starts the worker of an `ActivityExport` command; its ticket is answered when the file is written or failed.
pub(super) fn spawn(inner: &Arc<Inner>, ticket: Ticket, path: PathBuf, language: Language, utc_offset_min: i32) {
    let Some(out) = lock(&inner.out).clone() else {
        inner.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
        return;
    };
    let worker = Arc::clone(inner);
    let spawned = thread::Builder::new().name("swapdesk-ui-export".into()).spawn(move || {
        let result =
            export(&worker, &out, &path, language, utc_offset_min).map(|()| Reply::Done).map_err(ApiError::new);
        worker.reply(ticket, result);
    });
    if spawned.is_err() {
        inner.reply(ticket, Err(ApiError::new(ApiCode::Internal)));
    }
}

/// The newest entries the service keeps (at most [`MAX_ACTIVITY`]), written to `path`.
fn export(
    inner: &Inner,
    out: &mpsc::Sender<Item>,
    path: &Path,
    language: Language,
    utc_offset_min: i32,
) -> Result<(), ApiCode> {
    let limit = u32::try_from(MAX_ACTIVITY).unwrap_or(u32::MAX);
    let rows = match elevation::request(inner, out, UiOp::ActivityList { before_ms: None, limit })? {
        ReplyBody::Activity(rows) => rows,
        _ => return Err(ApiCode::Internal),
    };
    let rows =
        rows.into_iter().map(convert::audit_from_wire).collect::<Result<Vec<_>, _>>().map_err(|_| ApiCode::Internal)?;
    let csv = crate::format::activity_csv(language, &rows, utc_offset_min);
    std::fs::write(path, csv).map_err(|_| ApiCode::Internal)
}
