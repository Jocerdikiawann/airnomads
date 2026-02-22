use crate::error::Result;
use std::path::PathBuf;

pub struct QuicConfig {
    pub max_idle_timeout: u64,
    pub max_recv_udp_payload_size: usize,
    pub max_send_udp_payload_size: usize,
    pub initial_max_data: u64,
    pub initial_max_stream_data_bidi_local: u64,
    pub initial_max_stream_data_bidi_remote: u64,
    pub initial_max_stream_data_uni: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub disable_active_migration: bool,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    /// for client: skip verify (dev only!)
    pub verify_peer: bool,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            max_idle_timeout: 30_000,
            max_recv_udp_payload_size: 65535,
            max_send_udp_payload_size: 1350,
            initial_max_data: 10_000_000,
            initial_max_stream_data_bidi_local: 1_000_000,
            initial_max_stream_data_bidi_remote: 1_000_000,
            initial_max_stream_data_uni: 1_000_000,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 100,
            disable_active_migration: true,
            cert_path: None,
            key_path: None,
            verify_peer: true,
        }
    }
}

impl QuicConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build_quiche_config(&self) -> Result<quiche::Config> {
        let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;

        config.set_max_idle_timeout(self.max_idle_timeout);
        config.set_max_recv_udp_payload_size(self.max_recv_udp_payload_size);
        config.set_max_send_udp_payload_size(self.max_send_udp_payload_size);
        config.set_initial_max_data(self.initial_max_data);
        config.set_initial_max_stream_data_bidi_local(self.initial_max_stream_data_bidi_local);
        config.set_initial_max_stream_data_bidi_remote(self.initial_max_stream_data_bidi_remote);
        config.set_initial_max_stream_data_uni(self.initial_max_stream_data_uni);
        config.set_initial_max_streams_bidi(self.initial_max_streams_bidi);
        config.set_initial_max_streams_uni(self.initial_max_streams_uni);
        config.set_disable_active_migration(self.disable_active_migration);
        config.enable_dgram(true, 1000, 1000); // Enable quic DATAGRAM untuk realtime
        config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;

        if let (Some(cert), Some(key)) = (&self.cert_path, &self.key_path) {
            config.load_cert_chain_from_pem_file(cert.to_str().unwrap())?;
            config.load_priv_key_from_pem_file(key.to_str().unwrap())?;
        }

        if !self.verify_peer {
            config.verify_peer(false);
        }

        Ok(config)
    }

    pub fn with_cert(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        self.cert_path = Some(cert.into());
        self.key_path = Some(key.into());
        self
    }

    pub fn with_no_verify(mut self) -> Self {
        self.verify_peer = false;
        self
    }

    pub fn with_max_streams_bidi(mut self, n: u64) -> Self {
        self.initial_max_streams_bidi = n;
        self
    }
}
