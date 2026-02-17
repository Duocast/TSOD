#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub max_members_default: Option<i32>,
    pub max_talkers_default: Option<i32>,
}
