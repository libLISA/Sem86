use std::collections::HashMap;
use std::net::Ipv4Addr;

use log::{info, warn};
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::udp::{PacketBuffer, PacketMetadata, RecvError, Socket, UdpMetadata};
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, IpAddress, Ipv4Address};

pub struct DhcpServer {
    socket: SocketHandle,
    packets_received: usize,
    next_ip: u8,
    assigned_ips: HashMap<[u8; 6], Ipv4Address>,
    server_ip: Ipv4Address,
    subnet_mask: Ipv4Address,
    router: Ipv4Address,
    dns_servers: Vec<Ipv4Address>,
}

impl DhcpServer {
    pub fn new(sockets: &mut SocketSet) -> Self {
        let rx_metadata = vec![PacketMetadata::EMPTY; 16];
        let rx_data = vec![0u8; 16 * 512];

        let tx_metadata = vec![PacketMetadata::EMPTY; 16];
        let tx_data = vec![0u8; 16 * 512];

        let mut socket = Socket::new(
            PacketBuffer::new(rx_metadata, rx_data),
            PacketBuffer::new(tx_metadata, tx_data),
        );
        socket.bind((IpAddress::v4(10, 0, 37, 1), 67)).unwrap();
        let handle = sockets.add(socket);

        Self {
            socket: handle,
            packets_received: 0,
            next_ip: 15,
            assigned_ips: HashMap::new(),
            server_ip: Ipv4Address::new(10, 0, 37, 1),
            subnet_mask: Ipv4Address::new(255, 255, 255, 0),
            router: Ipv4Address::new(10, 0, 37, 1),
            dns_servers: vec![Ipv4Address::new(8, 8, 8, 8)],
        }
    }

    fn allocate_ip(&mut self, mac: [u8; 6]) -> Ipv4Address {
        if let Some(&ip) = self.assigned_ips.get(&mac) {
            return ip;
        }
        let ip = Ipv4Address::new(10, 0, 37, self.next_ip);
        self.assigned_ips.insert(mac, ip);
        self.next_ip += 1;
        ip
    }

    pub fn update(&mut self, sockets: &mut SocketSet) {
        let socket: &mut Socket = sockets.get_mut(self.socket);
        loop {
            match socket.recv() {
                Ok((bytes, metadata)) => {
                    self.packets_received += 1;

                    if let Ok(packet) = DhcpPacket::new_checked(bytes) {
                        let request = DhcpRepr::parse(&packet).unwrap();
                        info!("DHCP Request: {request:#X?}");

                        let client_mac = request.client_hardware_address.as_bytes();
                        match request.message_type {
                            DhcpMessageType::Discover => {
                                let your_ip = self.allocate_ip(client_mac.try_into().unwrap());

                                let offer = DhcpRepr {
                                    message_type: DhcpMessageType::Offer,
                                    transaction_id: request.transaction_id,
                                    client_hardware_address: request.client_hardware_address,
                                    client_ip: Ipv4Address::UNSPECIFIED,
                                    your_ip,
                                    server_ip: self.server_ip,
                                    router: Some(self.router),
                                    subnet_mask: Some(self.subnet_mask),
                                    broadcast: true,
                                    requested_ip: None,
                                    client_identifier: Some(request.client_hardware_address),
                                    server_identifier: Some(self.server_ip),
                                    parameter_request_list: request.parameter_request_list,
                                    dns_servers: Some(self.dns_servers.iter().copied().collect()),
                                    max_size: None,
                                    lease_duration: Some(3600),
                                    renew_duration: None,
                                    rebind_duration: None,
                                    additional_options: &[],
                                    secs: 0,
                                    relay_agent_ip: Ipv4Addr::UNSPECIFIED,
                                };

                                info!("Sending DHCP offer: {offer:#X?}");
                                let mut response_buf = [0u8; 512];
                                let mut response_pkt = DhcpPacket::new_unchecked(&mut response_buf[..offer.buffer_len()]);
                                offer.emit(&mut response_pkt).unwrap();
                                socket
                                    .send_slice(
                                        response_pkt.into_inner(),
                                        UdpMetadata {
                                            endpoint: (Ipv4Address::BROADCAST, metadata.endpoint.port).into(),
                                            local_address: Some(self.server_ip.into()),
                                            meta: metadata.meta,
                                        },
                                    )
                                    .unwrap();
                            },
                            DhcpMessageType::Request => {
                                let your_ip = packet
                                    .options()
                                    .find(|o| o.kind == 0x32)
                                    .map(|o| {
                                        assert_eq!(o.data.len(), 4);
                                        // TODO: Make sure IP is within our subnet
                                        Ipv4Address::new(o.data[0], o.data[1], o.data[2], o.data[3])
                                    })
                                    .unwrap_or_else(|| self.allocate_ip(client_mac.try_into().unwrap()));

                                let response = DhcpRepr {
                                    message_type: DhcpMessageType::Ack,
                                    transaction_id: packet.transaction_id(),
                                    client_hardware_address: request.client_hardware_address,
                                    client_ip: Ipv4Address::UNSPECIFIED,
                                    your_ip,
                                    server_ip: self.server_ip,
                                    router: Some(self.router),
                                    subnet_mask: Some(self.subnet_mask),
                                    broadcast: true,
                                    requested_ip: None,
                                    client_identifier: Some(request.client_hardware_address),
                                    server_identifier: Some(self.server_ip),
                                    parameter_request_list: request.parameter_request_list,
                                    dns_servers: Some(self.dns_servers.iter().copied().collect()),
                                    max_size: None,
                                    lease_duration: Some(3600),
                                    renew_duration: None,
                                    rebind_duration: None,
                                    additional_options: &[],
                                    secs: 0,
                                    relay_agent_ip: Ipv4Addr::UNSPECIFIED,
                                };

                                let mut response_buf = [0u8; 512];
                                let mut response_pkt = DhcpPacket::new_unchecked(&mut response_buf[..response.buffer_len()]);
                                response.emit(&mut response_pkt).unwrap();
                                socket
                                    .send_slice(
                                        response_pkt.into_inner(),
                                        UdpMetadata {
                                            endpoint: (Ipv4Address::BROADCAST, metadata.endpoint.port).into(),
                                            local_address: Some(self.server_ip.into()),
                                            meta: metadata.meta,
                                        },
                                    )
                                    .unwrap();
                            },
                            DhcpMessageType::Inform => {
                                let response = DhcpRepr {
                                    message_type: DhcpMessageType::Ack,
                                    transaction_id: request.transaction_id,
                                    client_hardware_address: request.client_hardware_address,
                                    client_ip: request.client_ip,
                                    your_ip: Ipv4Address::UNSPECIFIED,
                                    server_ip: self.server_ip,
                                    subnet_mask: Some(self.subnet_mask),
                                    router: Some(self.router),
                                    dns_servers: Some(self.dns_servers.iter().copied().collect()),
                                    broadcast: request.broadcast,
                                    requested_ip: None,
                                    client_identifier: request.client_identifier,
                                    server_identifier: Some(self.server_ip),
                                    parameter_request_list: request.parameter_request_list,
                                    max_size: None,
                                    lease_duration: None,
                                    renew_duration: None,
                                    rebind_duration: None,
                                    additional_options: &[],
                                    secs: 0,
                                    relay_agent_ip: Ipv4Addr::UNSPECIFIED,
                                };

                                let mut response_buf = [0u8; 512];
                                let mut response_pkt = DhcpPacket::new_unchecked(&mut response_buf[..response.buffer_len()]);
                                response.emit(&mut response_pkt).unwrap();
                                socket
                                    .send_slice(
                                        response_pkt.into_inner(),
                                        UdpMetadata {
                                            endpoint: metadata.endpoint,
                                            local_address: Some(self.server_ip.into()),
                                            meta: metadata.meta,
                                        },
                                    )
                                    .unwrap();
                            },
                            other => warn!("Ignored DHCP message {other:?}: {request:?}"),
                        }
                    }
                },
                Err(RecvError::Exhausted) => break,
                Err(RecvError::Truncated) => todo!("UDP message truncated"),
            }
        }
    }

    pub fn packets_received(&self) -> usize {
        self.packets_received
    }
}
