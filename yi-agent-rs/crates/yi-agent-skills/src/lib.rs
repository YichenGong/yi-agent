mod model;
mod loader;
mod discovery;
mod service;
mod system;

pub use model::{SkillMetadata, SkillScope, SkillError};
pub use service::SkillsService;
pub use system::install_system_skills;
