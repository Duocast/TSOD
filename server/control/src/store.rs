use parking_lot::RwLock;
use std::sync::Arc;

use crate::{
    channels::ChannelService,
    membership::MembershipService,
    perms::PermissionDb,
    ServerId,
};

#[derive(Clone)]
pub struct InMemoryStore {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    channels: ChannelService,
    membership: MembershipService,
}

impl InMemoryStore {
    pub fn new(server_id: ServerId) -> Self {
        let perms = PermissionDb::new_with_defaults();
        let channels = ChannelService::new(server_id, perms);
        let membership = MembershipService::new();
        Self {
            inner: Arc::new(RwLock::new(Inner { channels, membership })),
        }
    }

    pub fn with_read<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
        let g = self.inner.read();
        f(&g)
    }

    pub fn with_write<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut g = self.inner.write();
        f(&mut g)
    }
}
