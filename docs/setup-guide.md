# TSOD Setup Guide

Deploy the `vp-gateway` server on Ubuntu Server 24.04 and connect the `vp-client`
from a Windows 11 desktop. Covers LAN and external access through a Ubiquiti
Dream Machine SE gateway with full TLS using a custom Certificate Authority.

---

## Architecture

```
 Windows 11 Desktop                   Ubuntu Server 24.04 (Proxmox VM)
+-----------------+                  +--------------------------------+
|   vp-client     | ---QUIC/UDP----> |  vp-gateway (:4433)            |
|   (TUI + audio) |     :4433       |    |                            |
+-----------------+                  |    +-> PostgreSQL (:5432)       |
                                     |    +-> Metrics HTTP (:9100)     |
                                     +--------------------------------+
                                                    |
                                     Ubiquiti Dream Machine SE
                                     (port forward UDP 4433 for
                                      external access)
```

### Components

| Component | Binary | Description |
|-----------|--------|-------------|
| **vp-gateway** | `server/gateway/` | QUIC voice gateway. Handles auth, channels, media forwarding, chat. Requires PostgreSQL. |
| **vp-client** | `client/` | Terminal UI client. Captures mic audio, encodes with Opus, sends/receives voice over QUIC datagrams. |

### Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 4433 | UDP | QUIC transport (voice + control) |
| 9100 | TCP | Prometheus metrics endpoint |
| 5432 | TCP | PostgreSQL (server-local only) |

---

## Part 1: Server Setup (Ubuntu Server 24.04 on Proxmox)

### 1.1 System Dependencies

```bash
sudo apt update
sudo apt install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  libopus-dev \
  protobuf-compiler \
  libasound2-dev \
  git \
  curl
```

Verify protoc is installed:

```bash
protoc --version
# libprotoc 3.x or higher
```

### 1.2 Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Select the default installation (option 1). Then load the environment:

```bash
source "$HOME/.cargo/env"
```

Verify:

```bash
rustc --version
cargo --version
```

### 1.3 Install PostgreSQL 16

Add the official PostgreSQL APT repository:

```bash
sudo apt install -y gnupg2 lsb-release

# Import the repository signing key
curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc | \
  sudo gpg --dearmor -o /usr/share/keyrings/postgresql-keyring.gpg

# Add the repository
echo "deb [signed-by=/usr/share/keyrings/postgresql-keyring.gpg] \
  http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" | \
  sudo tee /etc/apt/sources.list.d/pgdg.list

sudo apt update
sudo apt install -y postgresql-16
```

Start and enable the service:

```bash
sudo systemctl enable postgresql
sudo systemctl start postgresql
```

Create the database and user:

```bash
sudo -u postgres psql <<SQL
CREATE USER vp WITH PASSWORD 'changeme';
CREATE DATABASE vp OWNER vp;
GRANT ALL PRIVILEGES ON DATABASE vp TO vp;
SQL
```

> **Important:** Replace `changeme` with a strong password. This password goes
> into the `--database-url` connection string.

Verify connectivity:

```bash
psql "postgres://vp:changeme@localhost/vp" -c "SELECT 1;"
```

### 1.4 Clone and Build

```bash
git clone <your-repo-url> ~/tsod
cd ~/tsod/server/gateway
cargo build --release
```

The binary will be at `~/tsod/server/gateway/target/release/vp-gateway`.

> **Note:** The first build compiles all dependencies and may take several
> minutes. Subsequent builds are incremental and much faster.

### 1.5 TLS Setup with Custom CA

For production-grade TLS, create a local Certificate Authority and sign a
server certificate. This lets the client validate the server without trusting
every self-signed cert.

#### 1.5.1 Create a directory for certificates

```bash
sudo mkdir -p /etc/tsod/tls
```

#### 1.5.2 Generate the CA key and certificate

```bash
# CA private key (keep this secure)
sudo openssl genrsa -out /etc/tsod/tls/ca.key 4096

# CA certificate (valid 10 years)
sudo openssl req -x509 -new -nodes \
  -key /etc/tsod/tls/ca.key \
  -sha256 \
  -days 3650 \
  -out /etc/tsod/tls/ca.crt \
  -subj "/CN=TSOD Internal CA/O=TSOD"
```

#### 1.5.3 Generate the server certificate

Create a config file with Subject Alternative Names. Replace the IP addresses
and hostnames to match your environment:

```bash
sudo tee /etc/tsod/tls/server.cnf <<'EOF'
[req]
default_bits       = 2048
distinguished_name = req_dn
req_extensions     = v3_req
prompt             = no

[req_dn]
CN = tsod-server

[v3_req]
subjectAltName = @alt_names

[alt_names]
# LAN IP of the Ubuntu server VM
IP.1   = 192.168.1.100
# Localhost (for local testing)
IP.2   = 127.0.0.1
# Hostname
DNS.1  = tsod-server
# DDNS hostname (if configured for external access)
# DNS.2 = tsod.example.com
EOF
```

> **Important:** Edit `IP.1` to match your server's actual LAN IP. If you set
> up DDNS for external access (see Part 4), uncomment and set `DNS.2`.

Generate the server key and certificate signing request:

```bash
sudo openssl genrsa -out /etc/tsod/tls/server.key 2048
sudo openssl req -new \
  -key /etc/tsod/tls/server.key \
  -out /etc/tsod/tls/server.csr \
  -config /etc/tsod/tls/server.cnf
```

Sign with the CA:

```bash
sudo openssl x509 -req \
  -in /etc/tsod/tls/server.csr \
  -CA /etc/tsod/tls/ca.crt \
  -CAkey /etc/tsod/tls/ca.key \
  -CAcreateserial \
  -out /etc/tsod/tls/server.crt \
  -days 825 \
  -sha256 \
  -extensions v3_req \
  -extfile /etc/tsod/tls/server.cnf
```

Verify the certificate:

```bash
openssl x509 -in /etc/tsod/tls/server.crt -text -noout | grep -A2 "Subject Alternative Name"
```

You should see your IP addresses and DNS names listed.

#### 1.5.4 Set permissions

```bash
sudo chown root:root /etc/tsod/tls/*
sudo chmod 600 /etc/tsod/tls/*.key
sudo chmod 644 /etc/tsod/tls/*.crt
```

### 1.6 Run the Server

```bash
~/tsod/server/gateway/target/release/vp-gateway \
  --listen 0.0.0.0:4433 \
  --database-url "postgres://vp:changeme@localhost/vp" \
  --tls-cert-pem /etc/tsod/tls/server.crt \
  --tls-key-pem /etc/tsod/tls/server.key
```

You should see:

```
INFO vp_gateway: listening on 0.0.0.0:4433
```

The server runs SQL migrations automatically on startup. No manual migration
step is needed.

#### Environment variable alternative

Instead of passing `--database-url` on the command line, you can export
`VP_DATABASE_URL`:

```bash
export VP_DATABASE_URL="postgres://vp:changeme@localhost/vp"
~/tsod/server/gateway/target/release/vp-gateway \
  --listen 0.0.0.0:4433 \
  --tls-cert-pem /etc/tsod/tls/server.crt \
  --tls-key-pem /etc/tsod/tls/server.key
```

### 1.7 Firewall (ufw)

```bash
# QUIC transport (required)
sudo ufw allow 4433/udp comment "TSOD QUIC"

# Prometheus metrics (optional, restrict as needed)
sudo ufw allow 9100/tcp comment "TSOD metrics"

sudo ufw enable
sudo ufw status
```

### 1.8 Optional: systemd Service

Create a service unit for automatic startup:

```bash
sudo tee /etc/systemd/system/tsod-gateway.service <<'EOF'
[Unit]
Description=TSOD Voice Gateway
After=network.target postgresql.service
Requires=postgresql.service

[Service]
Type=simple
User=tsod
Group=tsod
Environment=VP_DATABASE_URL=postgres://vp:changeme@localhost/vp
Environment=RUST_LOG=info
ExecStart=/home/tsod/tsod/server/gateway/target/release/vp-gateway \
  --listen 0.0.0.0:4433 \
  --tls-cert-pem /etc/tsod/tls/server.crt \
  --tls-key-pem /etc/tsod/tls/server.key
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
```

Create a dedicated system user and enable:

```bash
sudo useradd -r -s /usr/sbin/nologin tsod
sudo systemctl daemon-reload
sudo systemctl enable tsod-gateway
sudo systemctl start tsod-gateway
sudo systemctl status tsod-gateway
```

> **Note:** If running as a non-root user, ensure the `tsod` user has read
> access to the TLS certificate and key files.

---

## Part 2: Client Setup (Windows 11)

### 2.1 Install Visual Studio Build Tools 2022

The Rust compiler on Windows requires a C/C++ linker. Download **Visual Studio
Build Tools 2022** from:

```
https://visualstudio.microsoft.com/visual-cpp-build-tools/
```

During installation, select the **"Desktop development with C++"** workload.
This provides `cl.exe`, `link.exe`, and the Windows SDK.

### 2.2 Install Rust

Download and run the installer from:

```
https://rustup.rs
```

Accept the default options. This installs `rustc`, `cargo`, and `rustup`.

Open a **new** terminal (Command Prompt or PowerShell) and verify:

```powershell
rustc --version
cargo --version
```

### 2.3 Install Protocol Buffers Compiler

Download the latest `protoc` release for Windows from:

```
https://github.com/protocolbuffers/protobuf/releases
```

Look for `protoc-XX.Y-win64.zip`. Extract it and add the `bin` directory to
your `PATH`:

1. Extract to `C:\protoc`
2. Add `C:\protoc\bin` to the system PATH (System Properties -> Environment
   Variables -> Path -> Edit -> New)

Verify in a new terminal:

```powershell
protoc --version
```

### 2.4 Install Opus Library

The `opus` Rust crate requires the Opus C library. The recommended approach is
**vcpkg**:

```powershell
# Clone vcpkg
git clone https://github.com/microsoft/vcpkg.git C:\vcpkg
cd C:\vcpkg
.\bootstrap-vcpkg.bat

# Install opus for 64-bit Windows
.\vcpkg.exe install opus:x64-windows
```

Set the required environment variables (System Properties -> Environment
Variables, or in PowerShell for the current session):

```powershell
# Persistent (run in elevated PowerShell)
[Environment]::SetEnvironmentVariable("VCPKG_ROOT", "C:\vcpkg", "User")
[Environment]::SetEnvironmentVariable("VCPKGRS_DYNAMIC", "1", "User")
```

**Alternative:** If you have pre-built `opus.lib` and `opus.dll`, set
`OPUS_LIB_DIR` to the directory containing them.

### 2.5 Clone and Build

```powershell
git clone <your-repo-url> C:\tsod
cd C:\tsod\client
cargo build --release
```

The binary will be at `C:\tsod\client\target\release\vp-client.exe`.

### 2.6 Copy the CA Certificate

Copy `ca.crt` from the server to the Windows machine. For example, using `scp`:

```powershell
scp user@192.168.1.100:/etc/tsod/tls/ca.crt C:\tsod\ca.crt
```

Or transfer via USB, shared folder, etc.

### 2.7 Run the Client

#### LAN connection

```powershell
C:\tsod\client\target\release\vp-client.exe ^
  --server 192.168.1.100:4433 ^
  --server-name tsod-server ^
  --ca-cert-pem C:\tsod\ca.crt
```

#### External connection (after port forwarding, see Part 4)

```powershell
C:\tsod\client\target\release\vp-client.exe ^
  --server your-ddns.example.com:4433 ^
  --server-name tsod-server ^
  --ca-cert-pem C:\tsod\ca.crt
```

> **Important:** The `--server-name` value must match one of the SANs (Subject
> Alternative Names) in the server certificate. If you used `DNS.1 =
> tsod-server` in the server cert config, use `--server-name tsod-server`. If
> connecting by IP, ensure the IP was included as a SAN.

#### Optional flags

```
--channel-id <UUID>    Join a voice channel on connect
--dev-token dev        Auth token (default: "dev")
--push-to-talk         Enable push-to-talk (spacebar in TUI)
```

#### Alternative: Dev mode (skip TLS validation)

For quick LAN testing without CA certs, omit `--ca-cert-pem` and
`--server-name`. The client falls back to accepting any certificate (insecure):

```powershell
vp-client.exe --server 192.168.1.100:4433
```

#### Alternative: Certificate pinning

Instead of CA validation, you can pin the server's exact certificate hash:

```powershell
# On the server, get the cert pin:
openssl x509 -in /etc/tsod/tls/server.crt -outform DER | \
  sha256sum | awk '{print $1}'

# On the client, set the env var:
set VP_TLS_PIN_SHA256_HEX=<64-char-hex-hash>
vp-client.exe --server 192.168.1.100:4433 --server-name tsod-server
```

---

## Part 3: Network Configuration

### 3.1 Proxmox VM Networking

Ensure the Ubuntu Server VM uses **bridged networking** so it gets an IP on the
same subnet as the rest of your LAN:

1. In the Proxmox web UI, go to your VM -> **Hardware** -> **Network Device**
2. Verify the bridge is set to `vmbr0` (or your LAN bridge)
3. The VM should receive a LAN IP via DHCP or static config

Verify from the VM:

```bash
ip addr show
# Should show a 192.168.x.x address on the LAN interface
```

### 3.2 LAN Access

For devices on the same network, simply point the client at the VM's LAN IP:

```powershell
vp-client.exe --server 192.168.1.100:4433 --server-name tsod-server --ca-cert-pem C:\tsod\ca.crt
```

No port forwarding or firewall changes on the gateway are needed for LAN-only
access.

### 3.3 External Access via Ubiquiti Dream Machine SE

#### 3.3.1 Assign a static IP to the server

Reserve a fixed IP for the Ubuntu server VM so port forwarding rules remain
stable:

1. Open the **UniFi Network** app (https://unifi.ui.com or local gateway IP)
2. Go to **Client Devices** -> find the Ubuntu server -> click it
3. Under **Network**, toggle **Fixed IP Address**
4. Set the desired LAN IP (e.g., `192.168.1.100`)
5. Save. Reboot the VM or renew DHCP for the change to take effect

Alternatively, configure a static IP directly on the Ubuntu VM:

```bash
sudo tee /etc/netplan/01-static.yaml <<'EOF'
network:
  version: 2
  ethernets:
    ens18:    # adjust interface name (check with: ip link show)
      dhcp4: no
      addresses:
        - 192.168.1.100/24
      routes:
        - to: default
          via: 192.168.1.1    # your gateway IP
      nameservers:
        addresses:
          - 192.168.1.1
          - 1.1.1.1
EOF

sudo netplan apply
```

#### 3.3.2 Create a port forwarding rule

1. Open the **UniFi Network** app
2. Go to **Settings** -> **Firewall & Security** -> **Port Forwarding**
3. Click **Create New Port Forwarding Rule**:

| Field | Value |
|-------|-------|
| Name | TSOD Voice |
| Enabled | Yes |
| From | Any (or restrict to specific IPs) |
| Port | 4433 |
| Forward IP | 192.168.1.100 (your server's LAN IP) |
| Forward Port | 4433 |
| Protocol | UDP |

4. Save

> **Note:** UniFi OS 5.x / Network Application 10.1.x automatically creates
> the necessary WAN-In firewall allow rule when you add a port forwarding
> entry. You do not need to create a separate firewall rule.

#### 3.3.3 Optional: Restrict source IPs

If you want to allow only specific external IPs:

1. Go to **Settings** -> **Firewall & Security** -> **Firewall Rules** -> **WAN In**
2. Find the auto-created rule for port 4433
3. Edit it to restrict the source IP range

#### 3.3.4 DDNS for external access

If your ISP assigns a dynamic WAN IP, set up Dynamic DNS:

1. Sign up for a DDNS provider (e.g., DuckDNS, No-IP, Cloudflare DDNS)
2. Configure the DDNS client on your Dream Machine SE:
   - Go to **Settings** -> **Internet** -> your WAN connection
   - Under **Dynamic DNS**, enable it and enter your provider credentials
3. Your external hostname (e.g., `tsod.duckdns.org`) will auto-update with
   your WAN IP

> **Important:** If you set up DDNS, go back to Part 1, Section 1.5.3 and add
> the DDNS hostname as a SAN in the server certificate (`DNS.2`). Then
> regenerate and re-sign the server cert.

#### 3.3.5 Client connects externally

```powershell
vp-client.exe ^
  --server tsod.duckdns.org:4433 ^
  --server-name tsod-server ^
  --ca-cert-pem C:\tsod\ca.crt
```

---

## Part 4: Verification

### Server health check

From any machine on the LAN:

```bash
curl http://192.168.1.100:9100/metrics
```

You should see Prometheus-format metrics output.

### Client connection test

Run the client and watch for these log messages in the TUI:

```
[sys] starting, server=192.168.1.100:4433
[net] connected
[net] authed
```

If you provided `--channel-id`:

```
[ctl] joined channel <uuid>
```

### Voice test

1. Start the server
2. Start two clients with the same `--channel-id`
3. Speak into the microphone on one client
4. Audio should play out on the other client

### TLS verification

Verify the server is using your CA-signed cert:

```bash
# From the server itself
openssl s_client -connect 127.0.0.1:4433 -CAfile /etc/tsod/tls/ca.crt 2>/dev/null | \
  openssl x509 -noout -subject -issuer
```

---

## Part 5: Troubleshooting

### Connection timeout / no response

- **Firewall:** Verify UDP 4433 is open: `sudo ufw status | grep 4433`
- **Port forwarding:** For external access, verify with an external port
  checker that UDP 4433 is reachable on your WAN IP
- **QUIC uses UDP:** Many corporate/hotel networks block non-standard UDP ports.
  Try from a different network
- **Proxmox firewall:** Check that the Proxmox-level firewall (if enabled) also
  allows UDP 4433. Proxmox has its own firewall separate from the VM's ufw

### TLS errors

- **"cert pin mismatch"**: The `VP_TLS_PIN_SHA256_HEX` value doesn't match the
  server cert. Regenerate the pin hash
- **"invalid certificate" / "unknown CA"**: The `--ca-cert-pem` file doesn't
  contain the CA that signed the server cert. Verify the cert chain:
  ```bash
  openssl verify -CAfile /etc/tsod/tls/ca.crt /etc/tsod/tls/server.crt
  ```
- **Server name mismatch**: The `--server-name` must match a SAN in the server
  cert. Check with:
  ```bash
  openssl x509 -in /etc/tsod/tls/server.crt -text -noout | grep -A1 "Alternative"
  ```

### Build errors

- **"protoc not found"**: Ensure `protoc` is on your PATH. On Windows, open a
  new terminal after modifying PATH
- **Opus linking errors (Windows)**: Verify `VCPKG_ROOT` is set and
  `vcpkg install opus:x64-windows` completed successfully. Try setting
  `VCPKGRS_DYNAMIC=1`
- **Opus linking errors (Linux)**: Install `libopus-dev`:
  `sudo apt install libopus-dev`
- **ALSA errors (Linux server)**: Install `libasound2-dev`:
  `sudo apt install libasound2-dev`

### PostgreSQL connection errors

- **"connection refused"**: Verify PostgreSQL is running:
  `sudo systemctl status postgresql`
- **"authentication failed"**: Check the username/password in `--database-url`
- **"database does not exist"**: Run the database creation step from Section 1.3

### Audio issues

- **"no input device"**: The client requires a working microphone. Check that
  your audio device is connected and recognized by the OS
- **No audio playback**: Check that a default audio output device is configured
- **On the server**: The server does not need audio devices. It only forwards
  voice datagrams between clients

### Debug logging

Enable verbose logging on either the server or client:

```bash
RUST_LOG=debug ./vp-gateway ...    # Linux server
```

```powershell
set RUST_LOG=debug
vp-client.exe ...                  # Windows client
```

---

## Quick Reference

### Server command

```bash
vp-gateway \
  --listen 0.0.0.0:4433 \
  --database-url "postgres://vp:PASSWORD@localhost/vp" \
  --tls-cert-pem /etc/tsod/tls/server.crt \
  --tls-key-pem /etc/tsod/tls/server.key
```

### Client command (LAN)

```powershell
vp-client.exe --server 192.168.1.100:4433 --server-name tsod-server --ca-cert-pem C:\tsod\ca.crt
```

### Client command (external)

```powershell
vp-client.exe --server tsod.duckdns.org:4433 --server-name tsod-server --ca-cert-pem C:\tsod\ca.crt
```

### All server flags

```
--listen              Bind address (default: 0.0.0.0:4433)
--database-url        PostgreSQL URL (or VP_DATABASE_URL env var)
--tls-cert-pem        Path to TLS certificate PEM
--tls-key-pem         Path to TLS private key PEM
--alpn                ALPN protocol (default: vp-control/1)
--default-server-id   Server UUID (default: 00000000-0000-0000-0000-0000000000aa)
--metrics-listen      Metrics bind address (default: 0.0.0.0:9100)
--dev-mode            Accept dev auth tokens (default: true)
--max-connections     Max concurrent connections (default: 10000)
```

### All client flags

```
--server              Server address (default: 127.0.0.1:4433)
--server-name         TLS SNI name (default: localhost)
--ca-cert-pem         Path to CA certificate PEM (optional)
--alpn                ALPN protocol (default: vp-control/1)
--dev-token           Auth token (default: dev)
--channel-id          Channel UUID to join on connect (optional)
--push-to-talk        Enable push-to-talk mode (default: true)
```
