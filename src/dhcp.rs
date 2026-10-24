//! The lab's DHCP server hands each Zero Trust lease its address, for the lease's MAC, exactly as
//! it would to a real BMC. dpusimaas keeps the lease renewed for as long as the lease exists.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use dhcproto::{
    Decodable, Decoder, Encodable, Encoder,
    v4::{DhcpOption, Flags, Message, MessageType, OptionCode},
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, time::timeout};

use crate::{command::run, lease::Lease, netns};

const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
const ATTEMPTS: u32 = 3;
/// What a lease is taken to last when the server doesn't say.
const DEFAULT_LEASE_TIME: Duration = Duration::from_hours(1);

/// The address DHCP gave a lease.
pub struct DhcpLease {
    pub address: Ipv4Addr,
    pub prefix_len: u32,
    pub router: Option<Ipv4Addr>,
    pub lease_time: Duration,
}

/// Gets (or renews, asking for `previous`) the lease's address and puts it on the namespace's eth0.
pub async fn acquire_address(lease: &Lease, previous: Option<Ipv4Addr>) -> Result<DhcpLease> {
    let mac = parse_mac(&lease.mac)?;
    let client_socket = netns::run_in(&lease.id, open_client_socket)
        .await
        .context("opening a DHCP client socket")?;
    let client = DhcpClient {
        socket: UdpSocket::from_std(client_socket).context("registering the DHCP socket")?,
        mac,
        hostname: format!("dpusim-{}", lease.id),
    };
    let dhcp_lease = client
        .request(previous)
        .await
        .context("asking DHCP for an address")?;
    configure_eth0(&netns::namespace_name(&lease.id), &dhcp_lease).await?;
    Ok(dhcp_lease)
}

async fn configure_eth0(namespace: &str, dhcp_lease: &DhcpLease) -> Result<()> {
    let address = format!("{}/{}", dhcp_lease.address, dhcp_lease.prefix_len);
    let lifetime = dhcp_lease.lease_time.as_secs().to_string();
    run(
        "ip",
        [
            "-n",
            namespace,
            "addr",
            "replace",
            &address,
            "dev",
            "eth0",
            "valid_lft",
            &lifetime,
            "preferred_lft",
            &lifetime,
        ],
    )
    .await?;
    if let Some(router) = dhcp_lease.router {
        let router = router.to_string();
        run(
            "ip",
            [
                "-n", namespace, "route", "replace", "default", "via", &router,
            ],
        )
        .await?;
    }
    Ok(())
}

/// A UDP socket on the client port of the namespace's eth0, which can broadcast before eth0 has
/// an address. Must be opened inside the namespace.
fn open_client_socket() -> Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("creating a UDP socket")?;
    socket
        .set_reuse_address(true)
        .context("setting SO_REUSEADDR")?;
    socket.set_broadcast(true).context("setting SO_BROADCAST")?;
    socket
        .bind_device(Some(b"eth0"))
        .context("binding to eth0")?;
    socket
        .set_nonblocking(true)
        .context("making the socket non-blocking")?;
    let client_address = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, CLIENT_PORT);
    socket
        .bind(&client_address.into())
        .with_context(|| format!("binding {client_address}"))?;
    Ok(socket.into())
}

fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let octets = mac
        .split(':')
        .map(|octet| u8::from_str_radix(octet, 16))
        .collect::<Result<Vec<u8>, _>>()
        .with_context(|| format!("parsing MAC {mac}"))?;
    octets
        .try_into()
        .ok()
        .with_context(|| format!("MAC {mac} is not 6 octets"))
}

struct DhcpClient {
    socket: UdpSocket,
    mac: [u8; 6],
    hostname: String,
}

impl DhcpClient {
    /// The whole exchange (discover, offer, request, ack), as a client without an address does it.
    async fn request(&self, previous: Option<Ipv4Addr>) -> Result<DhcpLease> {
        let transaction_id = rand::random();
        let discover = self.message(transaction_id, MessageType::Discover, previous, None);
        let offer = self
            .exchange(&discover, |answer| {
                is_answer(answer, transaction_id, &[MessageType::Offer])
            })
            .await
            .context("discovering a DHCP server")?;
        let server = match offer.opts().get(OptionCode::ServerIdentifier) {
            Some(DhcpOption::ServerIdentifier(server)) => Some(*server),
            _ => None,
        };
        let request = self.message(
            transaction_id,
            MessageType::Request,
            Some(offer.yiaddr()),
            server,
        );
        let answer_types = [MessageType::Ack, MessageType::Nak];
        let ack = self
            .exchange(&request, |answer| {
                is_answer(answer, transaction_id, &answer_types)
            })
            .await
            .with_context(|| format!("requesting {}", offer.yiaddr()))?;
        if ack.opts().msg_type() == Some(MessageType::Nak) {
            bail!("the DHCP server refused {}", offer.yiaddr());
        }
        lease_from_ack(&ack)
    }

    fn message(
        &self,
        transaction_id: u32,
        message_type: MessageType,
        requested_address: Option<Ipv4Addr>,
        server: Option<Ipv4Addr>,
    ) -> Message {
        let mut message = Message::default();
        // The server can't unicast to an address the client doesn't have yet.
        message
            .set_xid(transaction_id)
            .set_flags(Flags::default().set_broadcast())
            .set_chaddr(&self.mac);
        let client_identifier = std::iter::once(1).chain(self.mac).collect();
        let options = [
            Some(DhcpOption::MessageType(message_type)),
            Some(DhcpOption::Hostname(self.hostname.clone())),
            // Client id = 1 (Ethernet) + MAC, as NetworkManager sends it.
            Some(DhcpOption::ClientIdentifier(client_identifier)),
            Some(DhcpOption::ParameterRequestList(vec![
                OptionCode::SubnetMask,
                OptionCode::Router,
                OptionCode::DomainNameServer,
                OptionCode::AddressLeaseTime,
            ])),
            requested_address.map(DhcpOption::RequestedIpAddress),
            server.map(DhcpOption::ServerIdentifier),
        ];
        for option in options.into_iter().flatten() {
            message.opts_mut().insert(option);
        }
        message
    }

    /// Broadcasts `message` until an answer `accept` takes comes back.
    async fn exchange(
        &self,
        message: &Message,
        accept: impl Fn(&Message) -> bool,
    ) -> Result<Message> {
        let encoded = encode(message)?;
        let servers = SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT);
        for _ in 0..ATTEMPTS {
            self.socket
                .send_to(&encoded, servers)
                .await
                .with_context(|| format!("sending to {servers}"))?;
            if let Ok(answer) = timeout(ANSWER_TIMEOUT, self.receive(&accept)).await {
                return answer;
            }
        }
        bail!(
            "no answer after {ATTEMPTS} tries of {}s",
            ANSWER_TIMEOUT.as_secs()
        )
    }

    async fn receive(&self, accept: &impl Fn(&Message) -> bool) -> Result<Message> {
        let mut datagram = vec![0_u8; 1500];
        loop {
            let (length, _): (usize, SocketAddr) = self
                .socket
                .recv_from(&mut datagram)
                .await
                .context("receiving from the DHCP socket")?;
            let answer = datagram
                .get(..length)
                .and_then(|received| Message::decode(&mut Decoder::new(received)).ok());
            if let Some(answer) = answer.filter(accept) {
                return Ok(answer);
            }
        }
    }
}

fn encode(message: &Message) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    message
        .encode(&mut Encoder::new(&mut encoded))
        .context("encoding a DHCP message")?;
    Ok(encoded)
}

fn is_answer(answer: &Message, transaction_id: u32, types: &[MessageType]) -> bool {
    answer.xid() == transaction_id
        && answer
            .opts()
            .msg_type()
            .is_some_and(|message_type| types.contains(&message_type))
}

fn lease_from_ack(ack: &Message) -> Result<DhcpLease> {
    let options = ack.opts();
    let mask = match options.get(OptionCode::SubnetMask) {
        Some(DhcpOption::SubnetMask(mask)) => *mask,
        _ => bail!(
            "the DHCP server gave {} without a subnet mask",
            ack.yiaddr()
        ),
    };
    let router = match options.get(OptionCode::Router) {
        Some(DhcpOption::Router(routers)) => routers.first().copied(),
        _ => None,
    };
    let lease_time = match options.get(OptionCode::AddressLeaseTime) {
        Some(DhcpOption::AddressLeaseTime(seconds)) => Duration::from_secs(u64::from(*seconds)),
        _ => DEFAULT_LEASE_TIME,
    };
    Ok(DhcpLease {
        address: ack.yiaddr(),
        prefix_len: u32::from(mask).count_ones(),
        router,
        lease_time,
    })
}

#[cfg(test)]
mod tests {
    use dhcproto::v4::Opcode;

    use super::*;

    fn ack(options: Vec<DhcpOption>) -> Message {
        let mut message = Message::default();
        message
            .set_opcode(Opcode::BootReply)
            .set_xid(7)
            .set_yiaddr(Ipv4Addr::new(10, 1, 2, 3));
        for option in options {
            message.opts_mut().insert(option);
        }
        message
    }

    #[test]
    fn reads_the_ack() -> Result<()> {
        let answer = ack(vec![
            DhcpOption::MessageType(MessageType::Ack),
            DhcpOption::SubnetMask(Ipv4Addr::new(255, 255, 252, 0)),
            DhcpOption::Router(vec![Ipv4Addr::new(10, 1, 0, 1)]),
            DhcpOption::AddressLeaseTime(600),
        ]);
        let decoded = Message::decode(&mut Decoder::new(&encode(&answer)?))?;
        assert!(is_answer(&decoded, 7, &[MessageType::Ack]));
        assert!(!is_answer(&decoded, 8, &[MessageType::Ack]));
        let dhcp_lease = lease_from_ack(&decoded)?;
        assert_eq!(dhcp_lease.address, Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(dhcp_lease.prefix_len, 22);
        assert_eq!(dhcp_lease.router, Some(Ipv4Addr::new(10, 1, 0, 1)));
        assert_eq!(dhcp_lease.lease_time, Duration::from_mins(10));
        Ok(())
    }

    #[test]
    fn lease_time_defaults_to_an_hour() -> Result<()> {
        let answer = ack(vec![DhcpOption::SubnetMask(Ipv4Addr::new(
            255, 255, 255, 0,
        ))]);
        assert_eq!(lease_from_ack(&answer)?.lease_time, DEFAULT_LEASE_TIME);
        Ok(())
    }

    #[test]
    fn parses_lease_macs() -> Result<()> {
        assert_eq!(
            parse_mac("52:5a:0a:0b:0c:0d")?,
            [0x52, 0x5a, 0x0a, 0x0b, 0x0c, 0x0d]
        );
        assert!(parse_mac("52:5a:0a").is_err());
        Ok(())
    }
}
