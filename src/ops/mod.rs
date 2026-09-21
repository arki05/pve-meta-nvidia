//! The verbs. Each takes a [`Ctx`] and does one thing a human could also do
//! by hand with `pve-meta`, an editor and `pct`.

pub mod daemon;
pub mod inventory;
pub mod reconcile;
pub mod status;

use anyhow::Result;

use crate::node;
use crate::state::Record;

/// What every verb needs: which node this is, and the record of the guests
/// this node wrote managed lines into.
pub struct Ctx {
    pub node: String,
    pub record: Record,
}

impl Ctx {
    pub fn load() -> Result<Self> {
        Ok(Ctx {
            node: node::nodename()?,
            record: Record::default(),
        })
    }
}
