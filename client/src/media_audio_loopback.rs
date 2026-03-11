pub trait AudioLoopbackBackend: Send + Sync {
    fn set_enabled(&self, enabled: bool);
    fn enabled(&self) -> bool;
}
