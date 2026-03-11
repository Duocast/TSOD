pub trait BitrateController: Send {
    fn target_bitrate_bps(&self) -> u32;
    fn update_target_bitrate_bps(&mut self, bitrate_bps: u32);
}
