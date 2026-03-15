use crate::screen_share::runtime_probe::nvidia;

#[derive(Clone, Debug, Default)]
pub struct Av1RuntimeCaps {
    pub nvenc_available: bool,
    pub nvenc_reason: Option<String>,
    pub rav1e_available: bool,
}

pub fn probe_av1_caps() -> Av1RuntimeCaps {
    let nv = nvidia::probe_nvenc_av1();
    Av1RuntimeCaps {
        nvenc_available: nv.available,
        nvenc_reason: nv.reason,
        rav1e_available: cfg!(feature = "video-av1-software"),
    }
}
