use super::*;

#[test]
fn watchman_home_is_the_real_home_not_the_instance_home() {
    assert_eq!(
        watchman_home_from(Some(std::ffi::OsStr::new("/Users/me"))),
        PathBuf::from("/Users/me")
    );
}

#[test]
fn skills_home_follows_the_instance_override_and_normalizes_it() {
    assert_eq!(
        skills_home_from(
            Some(std::ffi::OsStr::new("/tmp/dev-a")),
            Some(std::ffi::OsStr::new("/Users/me"))
        ),
        PathBuf::from("/tmp/dev-a/.yi-agent")
    );
}

#[test]
fn skills_home_falls_back_to_the_real_home_layout() {
    assert_eq!(
        skills_home_from(None, Some(std::ffi::OsStr::new("/Users/me"))),
        PathBuf::from("/Users/me/.yi-agent")
    );
}

#[test]
fn an_empty_override_falls_back_to_the_real_home_layout() {
    assert_eq!(
        skills_home_from(
            Some(std::ffi::OsStr::new("")),
            Some(std::ffi::OsStr::new("/Users/me"))
        ),
        PathBuf::from("/Users/me/.yi-agent")
    );
}
