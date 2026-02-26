use anyhow::Result;

pub struct OpusEncoder {
    enc: opus::Encoder,
}

pub struct OpusDecoder {
    dec: opus::Decoder,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch = if channels == 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        let enc = opus::Encoder::new(sample_rate, ch, opus::Application::Voip)?;
        Ok(Self { enc })
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        Ok(self.enc.encode(pcm, out)?)
    }
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch = if channels == 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        let dec = opus::Decoder::new(sample_rate, ch)?;
        Ok(Self { dec })
    }

    pub fn decode(&mut self, data: &[u8], pcm_out: &mut [i16]) -> Result<usize> {
        Ok(self.dec.decode(data, pcm_out, false)?)
    }
}
