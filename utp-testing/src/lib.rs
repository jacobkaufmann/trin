extern crate core;

pub mod cli;
pub mod rpc;

use crate::rpc::RpcServer;
use discv5::TalkRequest;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::http_server::{HttpServerBuilder, HttpServerHandle};
use jsonrpsee::proc_macros::rpc;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use trin_core::portalnet::discovery::{Discovery, UtpEnr};
use trin_core::portalnet::types::messages::{PortalnetConfig, ProtocolId};
use trin_core::portalnet::Enr;
use trin_core::utils::bytes::hex_encode;

/// uTP test app
pub struct TestApp {
    pub discovery: Arc<Discovery>,
    pub utp_socket: Arc<utp::socket::UtpSocket<UtpEnr>>,
    pub utp_talk_req_tx: mpsc::UnboundedSender<TalkRequest>,
    pub utp_payload: Arc<RwLock<Vec<Vec<u8>>>>,
}

#[async_trait]
impl RpcServer for TestApp {
    fn local_enr(&self) -> RpcResult<String> {
        Ok(self.discovery.local_enr().to_base64())
    }

    async fn get_utp_payload(&self) -> RpcResult<String> {
        let utp_payload = self.utp_payload.read().await;
        let utp_payload = utp_payload.last();

        match utp_payload {
            Some(payload) => Ok(hex_encode(payload)),
            None => Ok("false".to_string()),
        }
    }

    async fn prepare_to_recv(
        &self,
        src_enr: String,
        cid_send: u16,
        cid_recv: u16,
    ) -> RpcResult<String> {
        let src_enr = Enr::from_str(&src_enr).unwrap();
        let cid = utp::cid::ConnectionId {
            send: cid_send,
            recv: cid_recv,
            peer: UtpEnr(src_enr),
        };

        let utp = Arc::clone(&self.utp_socket);
        let payload_store = Arc::clone(&self.utp_payload);
        tokio::spawn(async move {
            let mut conn = utp.accept_with_cid(cid).await.unwrap();
            let mut data = vec![0; 4096 * 2 * 2];
            let n = conn.read_to_eof(&mut data).await.unwrap();

            payload_store.write().await.push(data[..n].to_vec());
        });

        Ok("true".to_string())
    }

    async fn send_utp_payload(
        &self,
        dst_enr: String,
        cid_send: u16,
        cid_recv: u16,
        payload: Vec<u8>,
    ) -> RpcResult<String> {
        let dst_enr = Enr::from_str(&dst_enr).unwrap();
        let cid = utp::cid::ConnectionId {
            send: cid_send,
            recv: cid_recv,
            peer: UtpEnr(dst_enr),
        };

        let utp = Arc::clone(&self.utp_socket);
        tokio::spawn(async move {
            let mut conn = utp.connect_with_cid(cid).await.unwrap();

            conn.write(&payload).await.unwrap();

            conn.shutdown().unwrap();
        });

        Ok("true".to_string())
    }
}

impl TestApp {
    pub async fn start(&self, mut talk_req_rx: mpsc::Receiver<TalkRequest>) {
        let utp_talk_reqs_tx = self.utp_talk_req_tx.clone();

        // Forward discv5 uTP packets to uTP socket
        tokio::spawn(async move {
            while let Some(request) = talk_req_rx.recv().await {
                let protocol_id =
                    ProtocolId::from_str(&hex::encode_upper(request.protocol())).unwrap();

                if let ProtocolId::Utp = protocol_id {
                    utp_talk_reqs_tx.send(request).unwrap();
                };
            }
        });
    }
}

/// Main method to spawn uTP Test App
pub async fn run_test_app(
    udp_port: u16,
    socket_addr: SocketAddr,
    rpc_addr: String,
    rpc_port: u16,
) -> anyhow::Result<(SocketAddr, Enr, HttpServerHandle)> {
    let config = PortalnetConfig {
        listen_port: udp_port,
        external_addr: Some(socket_addr),
        ..Default::default()
    };

    let mut discovery = Discovery::new(config).unwrap();
    let talk_req_rx = discovery.start().await.unwrap();
    let enr = discovery.local_enr();
    let discovery = Arc::new(discovery);

    let (utp_talk_req_tx, utp_talk_req_rx) = mpsc::unbounded_channel();
    let discv5_utp_socket = trin_core::portalnet::discovery::Discv5UdpSocket::new(
        Arc::clone(&discovery),
        utp_talk_req_rx,
    );
    let utp_socket = utp::socket::UtpSocket::with_socket(discv5_utp_socket);
    let utp_socket = Arc::new(utp_socket);

    let test_app = TestApp {
        discovery,
        utp_socket,
        utp_talk_req_tx,
        utp_payload: Arc::new(RwLock::new(Vec::new())),
    };

    test_app.start(talk_req_rx).await;

    let rpc_addr = format!("{rpc_addr}:{rpc_port}");

    // Start HTTP json-rpc server
    let server = HttpServerBuilder::default()
        .build(rpc_addr.parse::<SocketAddr>()?)
        .await?;

    let addr = server.local_addr()?;
    let handle = server.start(test_app.into_rpc()).unwrap();

    Ok((addr, enr, handle))
}
