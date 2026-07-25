mod discovery;
mod loader;
mod model;
mod service;
mod system;

pub use model::{SkillError, SkillMetadata, SkillScope};
pub use service::SkillsService;
pub use system::install_system_skills;
