use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(pub String);

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}

impl ChannelId {
    pub fn new() -> Self { Self(uuid::Uuid::new_v4().to_string()) }
}
