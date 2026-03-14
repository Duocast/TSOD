use anyhow::Result;

pub trait AudioLoopbackBackend: Send {
    fn backend_name(&self) -> &'static str;
    fn channels(&self) -> usize;
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self);
    fn read_frame(&mut self, pcm: &mut [i16]) -> bool;
}
