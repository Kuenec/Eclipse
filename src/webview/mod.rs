pub mod client;
pub mod fdpass;

pub(crate) mod hostprobe;
pub mod proto;
pub mod redact;
pub mod shm;
pub mod slots;

pub const PROTO_VERSION: u16 = 4;
