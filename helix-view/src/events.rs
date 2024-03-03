use helix_core::{ChangeSet, Rope};
use helix_event::events;

use crate::{Document, ViewId};

events! {
    DocumentDidChange<'a> {
        doc: &'a mut Document,
        view: ViewId,
        old_text: &'a Rope,
        changes: &'a ChangeSet,
        ghost_transaction: bool
    }
    SelectionDidChange<'a> { doc: &'a mut Document, view: ViewId }
}
