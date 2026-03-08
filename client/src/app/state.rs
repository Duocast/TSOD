#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct AppState {
    pub connected: bool,
    pub authed: bool,
    pub joined_channel: bool,
}
