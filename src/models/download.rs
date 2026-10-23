// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Model downloads into the Hugging Face cache through wavo's hub ([`Hub::pull`]). wavo resumes a
//! part an earlier pull left (`blobs/<etag>.incomplete`), retries a download that broke off and
//! checks the size.

use std::ops::ControlFlow;
use std::path::PathBuf;

use tracing::warn;
use wavo::hub::{Hub, ModelInfo, Progress};

/// Downloads `model` into `hub` on this thread, unless its latest revision is there already, and
/// returns the file's path. `on_progress` hears how far it got after every chunk received; a retry
/// is logged.
pub fn pull(
  hub: &Hub,
  model: &ModelInfo,
  mut on_progress: impl FnMut(Progress<'_>),
) -> wavo::Result<PathBuf> {
  hub.pull(model.name, |p| {
    if let Some(e) = p.retrying {
      warn!("{}: {e}; trying again", model.file());
    }

    on_progress(p);
    ControlFlow::Continue(())
  })
}
