use anyhow::Result;

pub struct OpusCodec {
    enc: opus::Encoder,
    dec: opus::Decoder,
}

impl OpusCodec {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch = if channels == 2 { opus::Channels::Stereo } else { opus::Channels::Mono };
        let enc = opus::Encoder::new(sample_rate, ch, opus::Application::Voip)?;
        let dec = opus::Decoder::new(sample_rate, ch)?;
        Ok(Self { enc, dec })
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        Ok(self.enc.encode(pcm, out)?)
    }

    pub fn decode(&mut self, data: &[u8], pcm_out: &mut [i16]) -> Result<usize> {
        Ok(self.dec.decode(data, pcm_out, false)?)
    }
}
