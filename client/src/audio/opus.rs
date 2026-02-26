use anyhow::Result;

pub struct OpusEncoder {
    enc: opus::Encoder,
    encoded_scratch: Vec<u8>,
}

pub struct OpusDecoder {
    dec: opus::Decoder,
    decoded_scratch: Vec<i16>,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch = if channels == 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        let enc = opus::Encoder::new(sample_rate, ch, opus::Application::Voip)?;
        Ok(Self {
            enc,
            encoded_scratch: vec![0u8; 4000],
        })
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        Ok(self.enc.encode(pcm, out)?)
    }

    pub fn encode_reuse(&mut self, pcm: &[i16]) -> Result<&[u8]> {
        let n = self.enc.encode(pcm, &mut self.encoded_scratch)?;
        Ok(&self.encoded_scratch[..n])
    }

    pub fn set_inband_fec(&mut self, enabled: bool) -> Result<()> {
        self.enc.set_inband_fec(enabled)?;
        Ok(())
    }

    pub fn set_packet_loss_perc(&mut self, loss_perc: i32) -> Result<()> {
        self.enc.set_packet_loss_perc(loss_perc.clamp(0, 100))?;
        Ok(())
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
        Ok(Self {
            dec,
            decoded_scratch: vec![0i16; (sample_rate as usize * 20 / 1000) * channels as usize],
        })
    }

    pub fn decode_reuse(&mut self, data: &[u8]) -> Result<&[i16]> {
        let n = self.dec.decode(data, &mut self.decoded_scratch, false)?;
        Ok(&self.decoded_scratch[..n])
    }

    pub fn decode(&mut self, data: &[u8], pcm_out: &mut [i16]) -> Result<usize> {
        Ok(self.dec.decode(data, pcm_out, false)?)
    }

    pub fn decode_plc(&mut self, pcm_out: &mut [i16]) -> Result<usize> {
        Ok(self.dec.decode(&[], pcm_out, false)?)
    }

    pub fn decode_fec(&mut self, data: &[u8], pcm_out: &mut [i16]) -> Result<usize> {
        Ok(self.dec.decode(data, pcm_out, true)?)
    }
}
