pub mod backups;
pub mod clusters;
pub(crate) mod cnpg;
pub(crate) mod cnpg_backups;
pub mod poolers;
mod scheduledbackups;

pub const VOLUME_SNAPSHOT_CLASS_NAME: &str = "cnpg-snapshot-class";
