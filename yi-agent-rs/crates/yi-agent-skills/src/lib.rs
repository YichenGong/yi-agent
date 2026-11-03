mod discovery;
mod loader;
mod model;
mod service;
mod system;

pub use model::{
    ManagedRoot, SkillConflict, SkillConflictEntry, SkillError, SkillMetadata, SkillScope,
    SkillSource, SkillVisibility,
};
pub use service::SkillsService;
pub use system::install_system_skills;
