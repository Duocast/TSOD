#[derive(Clone, Debug)]
pub struct AppState {
    pub connected: bool,
    pub authed: bool,
    pub joined_channel: bool,
}
