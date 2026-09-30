use std::path::{Path, PathBuf};

/// 拒绝提升的原因。判据是**文件存在**，而非模型声称已完成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionError {
    MissingSpec(PathBuf),
    MissingPlan(PathBuf),
    SameFile(PathBuf),
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromotionError::MissingSpec(path) => {
                write!(f, "spec file does not exist: {}", path.display())
            }
            PromotionError::MissingPlan(path) => {
                write!(f, "plan file does not exist: {}", path.display())
            }
            PromotionError::SameFile(path) => {
                write!(
                    f,
                    "spec and plan must be different files: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for PromotionError {}

/// 校验一对 spec + plan：两者都必须是存在的**文件**，且互不相同。
pub fn validate_promotion(spec: &Path, plan: &Path) -> Result<(), PromotionError> {
    if !spec.is_file() {
        return Err(PromotionError::MissingSpec(spec.to_path_buf()));
    }
    if !plan.is_file() {
        return Err(PromotionError::MissingPlan(plan.to_path_buf()));
    }
    if spec == plan {
        return Err(PromotionError::SameFile(spec.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_present_pair_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("feature.spec.md");
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(validate_promotion(&spec, &plan), Ok(()));
    }

    #[test]
    fn a_missing_spec_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("missing.spec.md");
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(
            validate_promotion(&spec, &plan),
            Err(PromotionError::MissingSpec(spec.clone()))
        );
    }

    #[test]
    fn a_missing_plan_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("feature.spec.md");
        let plan = dir.path().join("missing.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        assert_eq!(
            validate_promotion(&spec, &plan),
            Err(PromotionError::MissingPlan(plan.clone()))
        );
    }

    #[test]
    fn the_same_path_for_both_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let both = dir.path().join("same.md");
        std::fs::write(&both, "x").unwrap();
        assert_eq!(
            validate_promotion(&both, &both),
            Err(PromotionError::SameFile(both.clone()))
        );
    }

    #[test]
    fn a_directory_in_place_of_a_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec_dir = dir.path().join("spec_dir");
        std::fs::create_dir(&spec_dir).unwrap();
        let plan = dir.path().join("feature.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        assert_eq!(
            validate_promotion(&spec_dir, &plan),
            Err(PromotionError::MissingSpec(spec_dir.clone()))
        );
    }
}
