# TSOD
Voice client/server


# From repo root
cargo build -p vp-netem
cargo build -p vp-soak

# netem (Linux, needs sudo)
sudo ./target/debug/vp-netem --iface eth0 apply --loss 1.5 --delay 30ms --jitter 10ms --distribution normal

# soak (safe defaults require pin or --insecure)
VP_TLS_PIN_SHA256_HEX=... ./target/debug/vp-soak --server 127.0.0.1:4433 --concurrency 25 --duration-secs 600 --join-channel <uuid> --report-json soak.json
