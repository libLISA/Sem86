//! Userspace NAT.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use mio::event::Event;
use mio::net::{TcpStream, UdpSocket};
use mio::{Events, Interest, Poll, Token};
use nix::libc;
use slab::Slab;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer};
use smoltcp::socket::udp::{PacketBuffer, PacketMetadata, SendError as UdpSendError, Socket as SmolUdpSocket, UdpMetadata};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpCidr, IpEndpoint, Ipv4Packet, PrettyPrinter, TcpPacket,
    UdpPacket,
};

use crate::hw::net::dhcp::DhcpServer;

struct EmulatorDevice<F> {
    incoming_packets: VecDeque<Packet>,
    outgoing_packets: Sender<Packet>,
    outgoing_packet_available: F,
}

#[derive(Clone, Debug)]
pub struct Packet {
    data: Vec<u8>,
}

impl Packet {
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn pad_to_60(&mut self) {
        if self.data.len() < 60 {
            self.data.resize(60, 0);
        }
    }
}

impl smoltcp::phy::RxToken for Packet {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.data)
    }
}

impl<Q: FnMut()> smoltcp::phy::TxToken for &mut EmulatorDevice<Q> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0; len];
        let result = f(&mut buf);

        self.outgoing_packets
            .send(Packet {
                data: buf,
            })
            .unwrap();
        (self.outgoing_packet_available)();

        result
    }
}

impl<Q: FnMut()> Device for EmulatorDevice<Q> {
    type RxToken<'a>
        = Packet
    where
        Self: 'a;

    type TxToken<'a>
        = &'a mut Self
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(p) = self.incoming_packets.pop_front() {
            trace!("Sending packet via EmulatorDevice: {p:02X?}");
            Some((p, self))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(self)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();

        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = None;

        caps
    }
}

#[derive(Debug)]
pub struct UserspaceNat {
    outgoing_packets: Sender<Packet>,
    incoming_packets: Receiver<Packet>,
}

impl UserspaceNat {
    pub fn new(notify_packet_available: impl FnMut() + Send + Sync + 'static) -> Self {
        let config = Config::new(smoltcp::wire::HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
            0x02, 0x37, 0x37, 0x37, 0x37, 0x37,
        ])));
        let (tx_sender, tx_receiver) = channel();
        let (rx_sender, rx_receiver) = channel();
        let mut device = EmulatorDevice {
            incoming_packets: VecDeque::new(),
            outgoing_packets: rx_sender,
            outgoing_packet_available: notify_packet_available,
        };

        // TODO: Why do we need two separate entries for this?
        let gateway_ip = Ipv4Addr::new(10, 0, 37, 1);
        let internet_ip = Ipv4Addr::new(0, 0, 0, 1);

        let mut sockets = SocketSet::new(Vec::new());
        let mut iface = Interface::new(config, &mut device, Instant::now());
        let local_network = IpCidr::new(gateway_ip.into(), 24);

        iface.update_ip_addrs(|addrs| {
            // ARP
            addrs.push(local_network).unwrap();

            // Seems to be necessary to make smoltcp open sockets for internet addresses
            addrs.push(IpCidr::new(internet_ip.into(), 0)).unwrap();
        });

        assert!(iface.has_ip_addr(gateway_ip));

        iface.routes_mut().add_default_ipv4_route(internet_ip).unwrap();

        iface.set_any_ip(true);

        std::thread::Builder::new()
            .name(String::from("NAT"))
            .spawn(move || {
                let mut runner = Runner {
                    dhcp: DhcpServer::new(&mut sockets),
                    iface,
                    device,
                    sockets,
                    tx_receiver,
                    local_network,
                    backing_sockets: Slab::new(),
                    tcp_mapping: HashMap::new(),
                    udp_mapping: HashMap::new(),
                    poll: Poll::new().unwrap(),
                };

                runner.run();
            })
            .unwrap();

        UserspaceNat {
            incoming_packets: rx_receiver,
            outgoing_packets: tx_sender,
        }
    }

    pub fn send_packet(&mut self, packet: &[u8]) {
        info!(
            "Sending outgoing packet to internet:\n{}",
            PrettyPrinter::<EthernetFrame<&'static [u8]>>::new("OUT ", &packet)
        );
        self.outgoing_packets
            .send(Packet {
                data: packet.to_vec(),
            })
            .unwrap();
    }

    pub fn recv_packet(&mut self) -> Option<Packet> {
        self.incoming_packets.try_recv().ok().inspect(|packet| {
            info!(
                "Received incoming packet to emulator:\n{}",
                PrettyPrinter::<EthernetFrame<&'static [u8]>>::new("OUT ", &packet.data())
            );
        })
    }
}

struct Runner<F> {
    device: EmulatorDevice<F>,
    tx_receiver: Receiver<Packet>,
    sockets: SocketSet<'static>,
    iface: Interface,
    dhcp: DhcpServer,
    local_network: IpCidr,
    backing_sockets: Slab<BackingSocket>,
    tcp_mapping: HashMap<ConnectionIdentifiers, SocketId>,
    udp_mapping: HashMap<IpEndpoint, SocketId>,
    poll: Poll,
}

#[derive(Copy, Clone, Debug)]
struct SocketId(usize);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct ConnectionIdentifiers {
    local: IpEndpoint,
    remote: IpEndpoint,
}

enum BackingSocket {
    Tcp(TcpBackingSocket),
    Udp(UdpBackingSocket),
}

struct TcpBackingSocket {
    backend_socket: TcpStream,
    remote_is_ready: bool,
    local_socket: SocketHandle,
    conn_identifiers: ConnectionIdentifiers,
}

impl TcpBackingSocket {
    pub fn perform_writes(&mut self, local_socket: &mut TcpSocket) -> bool {
        if !self.remote_is_ready {
            debug!("Skipping send to {:?}, because remote isn't ready", self.backend_socket);
        }

        while self.remote_is_ready && local_socket.can_recv() {
            match local_socket
                .recv(|bytes| {
                    let result = self.backend_socket.write(bytes);
                    (result.as_ref().map(|&v| v).unwrap_or(0), result)
                })
                .unwrap()
            {
                Ok(0) => {
                    // TODO: Distinguish between remote closing and not having any bytes in transmit buffer
                    info!("Write to remote {:?} returned 0", self.conn_identifiers);
                    return true
                },
                Ok(n) => trace!("Wrote {n} bytes to remote"),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        return true
                    } else {
                        error!("Failed to send buffer to remote {:?}: {e}", self.conn_identifiers);
                        return false
                    }
                },
            }
        }

        true
    }

    pub fn perform_reads(&mut self, local_socket: &mut TcpSocket) -> bool {
        if !self.remote_is_ready {
            debug!("Skipping receive from {:?}, because remote isn't ready", self.backend_socket);
        }

        while self.remote_is_ready && local_socket.can_send() {
            match local_socket
                .send(|bytes| {
                    let result = self.backend_socket.read(bytes);
                    (result.as_ref().map(|&v| v).unwrap_or(0), result)
                })
                .unwrap()
            {
                Ok(0) => {
                    // TODO: Distinguish between remote closing and not having any bytes in transmit buffer
                    info!("Read from remote {:?} returned 0", self.conn_identifiers);
                    return true
                },
                Ok(n) => trace!("Read {n} bytes from remote"),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        return true
                    } else {
                        error!("Failed to read from to remote {:?}: {e}", self.conn_identifiers);
                        return false
                    }
                },
            }
        }

        true
    }
}

struct UdpBackingSocket {
    /// Maps local endpoints from the emulator to UDP sockets.
    /// Each local endpoint needs a different UDP socket to give it a different port.
    ///
    /// We cannot detect this beforehand, since each UDP packet may be sent to a different host.
    /// Additionally, UDP packets with different source ports may be sent to the same remote endpoint.
    ///
    /// We cannot bind a SmolUdpSocket to a specific source port, so we have to detect this as we go.
    backend_sockets: HashMap<IpEndpoint, UdpSocket>,
    local_socket: SocketHandle,
    conn_identifiers: ConnectionIdentifiers,
}

impl UdpBackingSocket {
    pub fn perform_writes(&mut self, local_socket: &mut SmolUdpSocket) {
        while local_socket.can_recv() {
            if let Ok((packet, metadata)) = local_socket.recv() {
                let remote = match self.conn_identifiers.remote.addr {
                    IpAddress::Ipv4(ipv4_addr) => SocketAddr::V4(SocketAddrV4::new(ipv4_addr, self.conn_identifiers.remote.port)),
                    IpAddress::Ipv6(ipv6_addr) => {
                        SocketAddr::V6(SocketAddrV6::new(ipv6_addr, self.conn_identifiers.remote.port, 0, 0))
                    },
                };

                match self.backend_sockets[&metadata.endpoint].send_to(packet, remote) {
                    Ok(n) => trace!("Sent {n}-byte UDP packet to remote {remote:?}"),
                    Err(e) => {
                        warn!("Error sending UDP packet: {e}");
                    },
                }
            }
        }
    }

    pub fn perform_reads(&mut self, local_socket: &mut SmolUdpSocket) {
        if !local_socket.can_send() {
            warn!("Failed to receive UDP packet, because local socket's transmit buffer is full");
        }

        let mut any_received = true;
        while any_received && local_socket.can_send() {
            let mut buf = [0; 4096];
            any_received = false;
            'next_socket: for (local_endpoint, socket) in self.backend_sockets.iter_mut() {
                match socket.recv_from(&mut buf) {
                    Ok((n, sender)) => {
                        match local_socket.send(
                            n,
                            UdpMetadata {
                                endpoint: *local_endpoint,
                                local_address: None,
                                meta: Default::default(),
                            },
                        ) {
                            Ok(send_buf) => {
                                trace!("Received {n}-byte UDP packet from remote {sender:?}");
                                send_buf.copy_from_slice(&buf[..n]);
                                any_received = true;
                            },
                            Err(UdpSendError::BufferFull) => (),
                            Err(UdpSendError::Unaddressable) => unreachable!(),
                        }
                    },
                    Err(e) => {
                        if e.kind() == ErrorKind::WouldBlock {
                            continue 'next_socket
                        } else {
                            panic!("Failed to read from to remote {:?}: {e}", self.conn_identifiers);
                        }
                    },
                }
            }
        }
    }
}

impl<F: FnMut()> Runner<F> {
    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024);
        loop {
            // TODO: Also poll `self.tx_receiver` here?
            match self.poll.poll(&mut events, Some(Duration::from_millis(10))) {
                Ok(_) => {
                    for event in events.iter() {
                        self.handle_event(event);
                    }
                },
                Err(e) if e.kind() == ErrorKind::Interrupted => (),
                Err(e) => panic!("MIO polling error: {e}"),
            }

            while let Ok(packet) = self.tx_receiver.try_recv() {
                self.inspect_packet_for_nat(&packet);

                if let Ok(eth) = EthernetFrame::new_checked(packet.data())
                    && let EthernetProtocol::Arp = eth.ethertype()
                    && let Ok(arp) = smoltcp::wire::ArpPacket::new_checked(eth.payload())
                    && let smoltcp::wire::ArpOperation::Request = arp.operation()
                    && let &[a, b, c, d] = arp.target_protocol_addr()
                    // If the guest is asking for an IP in the local subnet
                    && let target_ip = std::net::Ipv4Addr::new(a, b, c, d)
                    && self.local_network.contains_addr(&target_ip.into())
                    && let smoltcp::wire::IpAddress::Ipv4(gw) = self.local_network.address()
                    // But it's NOT asking for the gateway itself...
                    && target_ip != gw
                {
                    // Avoid having smoltcp respond to ARP requests for any hosts except itself
                    // This negates the effects of any_ip = true for ARP requests.
                } else {
                    self.device.incoming_packets.push_back(packet);
                }
            }

            match self.iface.poll(Instant::now(), &mut self.device, &mut self.sockets) {
                smoltcp::iface::PollResult::None => (),
                smoltcp::iface::PollResult::SocketStateChanged => {
                    self.dhcp.update(&mut self.sockets);

                    self.update_backing_sockets();
                },
            }
        }
    }

    fn inspect_packet_for_nat(&mut self, packet: &Packet) {
        let eth = EthernetFrame::new_checked(packet.data()).unwrap();
        if eth.ethertype() == EthernetProtocol::Ipv4
            && let Ok(ip) = Ipv4Packet::new_checked(eth.payload())
            && !self.local_network.contains_addr(&ip.dst_addr().into())
        {
            match ip.next_header() {
                smoltcp::wire::IpProtocol::Tcp => {
                    let tcp = TcpPacket::new_checked(ip.payload()).unwrap();

                    if tcp.syn() && !tcp.ack() {
                        info!(
                            "Ensuring NAT for new connection: {}",
                            PrettyPrinter::<TcpPacket<&'static [u8]>>::new("", &ip.payload())
                        );
                        self.ensure_tcp_socket_for(ConnectionIdentifiers {
                            local: (ip.src_addr(), tcp.src_port()).into(),
                            remote: (ip.dst_addr(), tcp.dst_port()).into(),
                        });
                    }
                },
                smoltcp::wire::IpProtocol::Udp => {
                    let udp = UdpPacket::new_checked(ip.payload()).unwrap();

                    if !ip.dst_addr().is_broadcast() {
                        info!(
                            "Ensuring NAT for UDP: {}",
                            PrettyPrinter::<UdpPacket<&'static [u8]>>::new("", &ip.payload())
                        );
                        self.ensure_udp_socket_for(ConnectionIdentifiers {
                            local: (ip.src_addr(), udp.src_port()).into(),
                            remote: (ip.dst_addr(), udp.dst_port()).into(),
                        });
                    }
                },
                other => error!(
                    "Ignoring NAT for {other:?}: {}",
                    PrettyPrinter::<Ipv4Packet<&'static [u8]>>::new("", &eth.payload())
                ),
            }
        }
    }

    fn ensure_tcp_socket_for(&mut self, conn: ConnectionIdentifiers) {
        if !self.tcp_mapping.contains_key(&conn) {
            info!("Opening TCP socket for {conn:?}");
            let remote = match conn.remote.addr {
                IpAddress::Ipv4(ipv4_addr) => SocketAddr::V4(SocketAddrV4::new(ipv4_addr, conn.remote.port)),
                IpAddress::Ipv6(ipv6_addr) => SocketAddr::V6(SocketAddrV6::new(ipv6_addr, conn.remote.port, 0, 0)),
            };
            let mut backend_socket = TcpStream::connect(remote).unwrap();
            let entry = self.backing_sockets.vacant_entry();
            self.tcp_mapping.insert(conn, SocketId(entry.key()));
            self.poll
                .registry()
                .register(
                    &mut backend_socket,
                    Token(entry.key()),
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();

            let rx_data = vec![0u8; 16 * 512];
            let tx_data = vec![0u8; 16 * 512];

            let mut local_socket = TcpSocket::new(SocketBuffer::new(rx_data), SocketBuffer::new(tx_data));
            local_socket.listen(conn.remote).unwrap();
            let local_socket = self.sockets.add(local_socket);

            self.tcp_mapping.insert(conn, SocketId(entry.key()));

            entry.insert(BackingSocket::Tcp(TcpBackingSocket {
                backend_socket,
                remote_is_ready: false,
                local_socket,
                conn_identifiers: conn,
            }));
        }
    }

    fn ensure_udp_socket_for(&mut self, conn: ConnectionIdentifiers) {
        let socket_id = *self.udp_mapping.entry(conn.remote).or_insert_with(|| {
            let rx_metadata = vec![PacketMetadata::EMPTY; 16];
            let rx_data = vec![0u8; 16 * 512];

            let tx_metadata = vec![PacketMetadata::EMPTY; 16];
            let tx_data = vec![0u8; 16 * 512];

            let mut local_socket = SmolUdpSocket::new(
                PacketBuffer::new(rx_metadata, rx_data),
                PacketBuffer::new(tx_metadata, tx_data),
            );
            local_socket.bind(conn.remote).unwrap();
            let local_socket = self.sockets.add(local_socket);

            let entry = self.backing_sockets.vacant_entry();
            let id = SocketId(entry.key());
            entry.insert(BackingSocket::Udp(UdpBackingSocket {
                backend_sockets: HashMap::new(),
                local_socket,
                conn_identifiers: conn,
            }));

            id
        });

        let socket = &mut self.backing_sockets[socket_id.0];
        let BackingSocket::Udp(socket) = socket else { unreachable!() };

        if let Entry::Vacant(v) = socket.backend_sockets.entry(conn.local) {
            info!("Opening UDP socket for {conn:?}");
            let mut backend_socket = UdpSocket::bind("0.0.0.0:0".parse().unwrap()).unwrap();
            self.poll
                .registry()
                .register(
                    &mut backend_socket,
                    Token(socket_id.0),
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();

            v.insert(backend_socket);
        }
    }

    fn handle_event(&mut self, event: &Event) {
        let socket_id = SocketId(event.token().0);
        let socket = &mut self.backing_sockets[socket_id.0];
        match socket {
            BackingSocket::Tcp(socket) => {
                trace!("Handling event {event:?} for TCP {:?}", socket.conn_identifiers);
                let local_socket = self.sockets.get_mut::<TcpSocket>(socket.local_socket);

                match Self::handle_tcp_client_event(event, socket, local_socket) {
                    Ok(true) => (),
                    Ok(false) | Err(_) => {
                        self.poll.registry().deregister(&mut socket.backend_socket).unwrap();
                        self.sockets.remove(socket.local_socket);
                        self.tcp_mapping.remove(&socket.conn_identifiers);
                        self.backing_sockets.remove(socket_id.0);
                    },
                }
            },
            BackingSocket::Udp(socket) => {
                trace!("Handling event {event:?} for UDP {:?}", socket.conn_identifiers);
                let local_socket = self.sockets.get_mut::<SmolUdpSocket>(socket.local_socket);

                match Self::handle_udp_client_event(event, socket, local_socket) {
                    Ok(true) => (),
                    Ok(false) | Err(_) => {
                        for socket in socket.backend_sockets.values_mut() {
                            self.poll.registry().deregister(socket).unwrap();
                        }

                        self.sockets.remove(socket.local_socket);
                        self.udp_mapping.remove(&socket.conn_identifiers.remote);
                        self.backing_sockets.remove(socket_id.0);
                    },
                }
            },
        }
    }

    fn update_backing_sockets(&mut self) {
        self.backing_sockets.retain(|_, backing_socket| {
            match backing_socket {
                BackingSocket::Tcp(backing_socket) => {
                    let local_socket = self.sockets.get_mut::<TcpSocket>(backing_socket.local_socket);

                    trace!(
                        "Checking socket {:?} state={:?}, may_recv={}, may_send={}",
                        backing_socket.conn_identifiers,
                        local_socket.state(),
                        local_socket.may_recv(),
                        local_socket.may_recv()
                    );
                    if !backing_socket.perform_reads(local_socket) || !backing_socket.perform_writes(local_socket) {
                        self.poll.registry().deregister(&mut backing_socket.backend_socket).unwrap();
                        self.sockets.remove(backing_socket.local_socket);
                        self.tcp_mapping.remove(&backing_socket.conn_identifiers);

                        return false
                    }
                },
                BackingSocket::Udp(backing_socket) => {
                    let local_socket = self.sockets.get_mut::<SmolUdpSocket>(backing_socket.local_socket);

                    trace!(
                        "Checking socket {:?} can_recv={}, can_send={}",
                        backing_socket.conn_identifiers,
                        local_socket.can_send(),
                        local_socket.can_recv()
                    );
                    backing_socket.perform_reads(local_socket);
                    backing_socket.perform_writes(local_socket);
                },
            }

            true
        });
    }

    fn handle_tcp_client_event(
        event: &Event, socket: &mut TcpBackingSocket, local_socket: &mut TcpSocket<'_>,
    ) -> Result<bool, std::io::Error> {
        match socket.backend_socket.take_error() {
            Ok(None) => (),
            Ok(Some(e)) | Err(e) => {
                error!("TODO: tcp error: {e}");
                local_socket.abort();
                return Ok(false)
            },
        }

        if !socket.remote_is_ready {
            match socket.backend_socket.peer_addr() {
                Ok(_) => (),
                Err(e) if e.kind() == ErrorKind::NotConnected || e.raw_os_error() == Some(libc::EINPROGRESS) => {
                    // Stream is not yet connected, wait for another event.
                    debug!("Connection to remote endpoint {:?} in progress: {e}", socket.conn_identifiers);
                    return Ok(true)
                },
                Err(e) => {
                    error!("unable to connect to endpoint {:?}: {e}", socket.conn_identifiers);
                    return Ok(false)
                },
            }

            info!("Remote for {:?} is ready", socket.conn_identifiers);
            socket.remote_is_ready = true;
            if !socket.perform_writes(local_socket) {
                return Ok(false)
            }
        }

        if (event.is_writable() || event.is_write_closed()) && !socket.perform_writes(local_socket) {
            return Ok(false)
        }

        if (event.is_readable() || event.is_read_closed()) && !socket.perform_reads(local_socket) {
            return Ok(false)
        }

        Ok(true)
    }

    fn handle_udp_client_event(
        event: &Event, socket: &mut UdpBackingSocket, local_socket: &mut SmolUdpSocket<'_>,
    ) -> Result<bool, std::io::Error> {
        for backend_socket in socket.backend_sockets.values_mut() {
            match backend_socket.take_error() {
                Ok(None) => (),
                Ok(Some(e)) | Err(e) => {
                    error!("TODO: udp error: {e}");
                    return Ok(false)
                },
            }
        }

        if event.is_writable() || event.is_write_closed() {
            socket.perform_writes(local_socket);
        }

        if event.is_readable() || event.is_read_closed() {
            socket.perform_reads(local_socket);
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_log::test;

    use crate::hw::net::unat::UserspaceNat;

    #[test]
    pub fn dhcp_message_is_received() {
        let packet = &[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xB0, 0xC4, 0x20, 0x00, 0x00, 0x00, 0x08, 0x00, 0x45, 0x00, 0x01, 0x48, 0x00,
            0x00, 0x00, 0x00, 0x80, 0x11, 0x39, 0xA6, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x44, 0x00, 0x43,
            0x01, 0x34, 0x41, 0xF8, 0x01, 0x01, 0x06, 0x00, 0x3E, 0x02, 0x3E, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xB0, 0xC4, 0x20, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x63, 0x82, 0x53, 0x63, 0x35, 0x01, 0x01,
            0x3D, 0x07, 0x01, 0xB0, 0xC4, 0x20, 0x00, 0x00, 0x00, 0x32, 0x04, 0x0A, 0x00, 0x02, 0x0F, 0x0C, 0x07, 0x50, 0x31,
            0x42, 0x38, 0x43, 0x37, 0x00, 0x37, 0x08, 0x01, 0x03, 0x06, 0x0F, 0x2C, 0x2E, 0x2F, 0x39, 0xFF, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let mut nat = UserspaceNat::new(|| ());
        nat.send_packet(packet);

        std::thread::sleep(Duration::from_millis(250));

        let response = nat.recv_packet();
        assert!(response.is_some(), "{response:#?}");
    }
}
