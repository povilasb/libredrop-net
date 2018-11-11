use bincode;
use futures::stream;
use get_if_addrs::{get_if_addrs, IfAddr};
use priv_prelude::*;
use std::io;
use std::net::SocketAddrV4;
use tokio::net::UdpSocket;

/// Tries given expression. Returns boxed stream error on failure.
macro_rules! try_bstream {
    ($e:expr) => {
        match $e {
            Ok(t) => t,
            Err(e) => return stream::iter_result(vec![Err(e)]).into_boxed(),
        }
    };
}

// TODO(povilas): use failure crate for errors
/// Peers discovery error.
#[derive(Debug)]
pub enum DiscoveryError {
    Io(io::Error),
    SerializeFailure(bincode::Error),
    InvalidResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum DiscoveryMsg {
    /// Request has sender's public key which should be used to encrypt response.
    Request(PublicEncryptKey),
    // TODO(povilas): include their public key
    /// Addresses that the peer is accessible with.
    Response(Vec<SocketAddr>),
}

impl DiscoveryMsg {
    /// Returns serialized but not encrypted peer discovery request.
    fn serialized_request(pk: PublicEncryptKey) -> Result<Vec<u8>, DiscoveryError> {
        let msg = DiscoveryMsg::Request(pk);
        bincode::serialize(&msg).map_err(DiscoveryError::SerializeFailure)
    }
}

/// Peer discovery server that listens for other peer requests and responds with the addresses
/// we're listening on so other peers could connect to us.
pub struct DiscoveryServer {
    listener: UdpSocket,
    /// Addresses peer discovery will respond with.
    our_addrs: Vec<SocketAddr>,
    port: u16,
    /// Clients still waiting for response.
    clients: Vec<(SocketAddr, PublicEncryptKey)>,
}

impl DiscoveryServer {
    /// Constructs new peer discovery server that listens for requests on a given port.
    pub fn new(port: u16, our_addrs: Vec<SocketAddr>) -> io::Result<Self> {
        let listener = UdpSocket::bind(&SocketAddr::V4(SocketAddrV4::new(ipv4!("0.0.0.0"), port)))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            our_addrs: our_addrs,
            port,
            clients: Vec::new(),
        })
    }

    /// Returns server port.
    pub fn port(&self) -> u16 {
        self.port
    }

    fn poll_requests(&mut self) -> io::Result<()> {
        let mut buf = vec![0u8; 65000];
        loop {
            match self.listener.poll_recv_from(&mut buf)? {
                Async::Ready((bytes_read, sender_addr)) => {
                    self.on_packet_recv(&buf[..bytes_read], sender_addr);
                }
                Async::NotReady => return Ok(()),
            }
        }
    }

    fn on_packet_recv(&mut self, buf: &[u8], sender_addr: SocketAddr) {
        match bincode::deserialize(buf) {
            Ok(DiscoveryMsg::Request(their_pk)) => self.clients.push((sender_addr, their_pk)),
            // TODO(povilas): prevent from DDOSing logs and put upper limit for logged buffer
            _ => warn!("Invalid peer discovery request: {:?}", buf),
        }
    }

    fn poll_send_responses(&mut self) -> io::Result<()> {
        while let Some((addr, their_pk)) = self.clients.pop() {
            let resp = if let Some(buf) = self.make_response(&their_pk) {
                buf
            } else {
                continue;
            };
            match self.listener.poll_send_to(&resp, &addr)? {
                Async::Ready(_bytes_sent) => (),
                Async::NotReady => {
                    self.clients.push((addr, their_pk));
                    break;
                }
            }
        }
        Ok(())
    }

    /// Encrypt response with their public key.
    fn make_response(&self, their_pk: &PublicEncryptKey) -> Option<Vec<u8>> {
        let resp = DiscoveryMsg::Response(self.our_addrs.clone());
        their_pk.anonymously_encrypt(&resp).ok()
    }
}

impl Future for DiscoveryServer {
    type Item = Void;
    type Error = io::Error;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        self.poll_requests()?;
        self.poll_send_responses()?;
        Ok(Async::NotReady)
    }
}

/// Search peers on LAN.
pub fn discover_peers(port: u16) -> impl Stream<Item = Vec<SocketAddr>, Error = DiscoveryError> {
    let broadcast_to = try_bstream!(broadcast_addrs(port).map_err(DiscoveryError::Io));
    let (our_pk, our_sk) = gen_encrypt_keypair();
    let request = try_bstream!(DiscoveryMsg::serialized_request(our_pk));

    stream::iter_ok(broadcast_to)
        .and_then(move |addr| {
            let sock = broadcast_sock().map_err(DiscoveryError::Io)?;
            Ok((sock, addr))
        }).and_then(move |(sock, addr)| {
            sock.send_dgram(request.clone(), &addr)
                .map_err(DiscoveryError::Io)
        }).and_then(|(sock, _buf)| sock.recv_dgram(vec![0; 65000]).map_err(DiscoveryError::Io))
        .and_then(move |(_sock, buf, bytes_read, _sender_addr)| {
            match our_sk.anonymously_decrypt(&buf[..bytes_read], &our_pk) {
                Ok(DiscoveryMsg::Response(their_addrs)) => Ok(their_addrs),
                _ => Err(DiscoveryError::InvalidResponse),
            }
        }).and_then(|their_addrs| Ok(their_addrs))
        .into_boxed()
}

// TODO(povilas): netsim test for this
/// Returns broadcast addresses for all network intefaces on the system.
fn broadcast_addrs(port: u16) -> io::Result<Vec<SocketAddr>> {
    let addrs = get_if_addrs()?;
    Ok(addrs
        .iter()
        .filter_map(|iface| match iface.addr {
            IfAddr::V4(ref ifv4_addr) => ifv4_addr.broadcast,
            IfAddr::V6(_) => None,
        }).map(move |ip| SocketAddr::V4(SocketAddrV4::new(ip, port)))
        .collect())
}

/// Creates new UDP socket with broadcast enabled.
fn broadcast_sock() -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind(&addr!("0.0.0.0:0"))?;
    sock.set_broadcast(true)?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamcrest2::prelude::*;
    use tokio::runtime::current_thread::Runtime;

    #[test]
    fn server_responds() {
        let mut evloop = unwrap!(Runtime::new());

        let server = unwrap!(DiscoveryServer::new(0, vec![addr!("192.168.1.100:1234")]));
        let server_addr = SocketAddr::V4(SocketAddrV4::new(ipv4!("127.0.0.1"), server.port()));
        let sock = unwrap!(UdpSocket::bind(&addr!("0.0.0.0:0")));

        let (our_pk, our_sk) = gen_encrypt_keypair();
        let request = unwrap!(DiscoveryMsg::serialized_request(our_pk));

        let send_req = sock
            .send_dgram(&request, &server_addr)
            .and_then(|(sock, _buf)| sock.recv_dgram(vec![0; 65000]))
            .map(|(_socket, buf, bytes_received, _from)| buf[..bytes_received].to_vec())
            .with_timeout(Duration::from_secs(2))
            .map(|buf_opt| {
                let buf = unwrap!(buf_opt);
                unwrap!(our_sk.anonymously_decrypt(&buf, &our_pk))
            }).while_driving(server.map_err(DiscoveryError::Io));

        match evloop.block_on(send_req) {
            Ok((DiscoveryMsg::Response(addrs), _server_task)) => {
                assert_that!(addrs, eq(vec![addr!("192.168.1.100:1234")]));
            }
            _ => panic!("Failed to send peer discovery request"),
        }
    }

    #[test]
    fn discover_peers_broadcasts_requests_on_lan_and_collects_peer_addresses() {
        let mut evloop = unwrap!(Runtime::new());

        let server = unwrap!(DiscoveryServer::new(
            0,
            vec![addr!("192.168.1.100:1234"), addr!("127.0.0.1:1234")]
        ));
        let server_port = server.port();

        let task = discover_peers(server_port)
            .collect()
            .while_driving(server.map_err(DiscoveryError::Io));

        match evloop.block_on(task) {
            Ok((their_addrs, _server_task)) => {
                assert_that!(
                    their_addrs,
                    eq(vec![vec![
                        addr!("192.168.1.100:1234"),
                        addr!("127.0.0.1:1234")
                    ]])
                );
            }
            _ => panic!("Peer discovery failed"),
        }
    }
}