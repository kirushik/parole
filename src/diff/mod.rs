//! The diff view and its state, copied from rune-kit at the pinned revision (MIT, © 2026
//! Anomaly) and changed for this window: it knows which lines have been on screen, puts a
//! note on its own line, and takes its colours from [`crate::ui`]. The patch model, word
//! diff and syntax highlighter are still rune-kit's.

// Parts of the kit's view this window doesn't use (syntax colours, comments on lines, the
// other fold row) are left as they are, so taking a later revision of it is a plain diff.
#[allow(dead_code)]
mod state;
#[allow(dead_code, clippy::too_many_arguments)]
mod view;

pub use state::{Annotation, DiffMode, DiffState, LineRef, MapKind};
pub use view::DiffView;
