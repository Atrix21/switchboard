//! Clipboard synchronization (Milestone 6).

use std::sync::Arc;

use crate::engine::Peer;
use crate::proto::Ctrl;

pub struct ClipSync;

impl ClipSync {
    pub fn on_peer_ready(&self, _p: &Arc<Peer>) {}
    pub fn on_ctrl(&self, _p: &Arc<Peer>, _m: Ctrl) {}
    pub fn on_data(&self, _p: &Arc<Peer>, _id: u64, _last: bool, _bytes: Vec<u8>) {}
}
