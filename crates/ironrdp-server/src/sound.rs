pub use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};

use crate::ServerEventSender;

pub trait SoundServerFactory: ServerEventSender + Send + Sync {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler>;
}
