pub mod voiceplatform {
    #[allow(dead_code)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/voiceplatform.v1.rs"));
    }
}
