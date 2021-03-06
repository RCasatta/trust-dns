// Copyright 2015-2018 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::net::{SocketAddr, Ipv4Addr};
use std::io;

use futures::{Async, Future, Poll, Stream};
use tokio_core::reactor::Handle;

use BufDnsStreamHandle;
use DnsStreamHandle;
use error::*;
use multicast::{MdnsQueryType, MdnsStream};
use multicast::mdns_stream::{MDNS_IPV4, MDNS_IPV6};

/// A UDP client stream of DNS binary packets
#[must_use = "futures do nothing unless polled"]
pub struct MdnsClientStream {
    mdns_stream: MdnsStream,
}

impl MdnsClientStream {
    /// associates the socket to the well-known ipv4 multicast addess
    pub fn new_ipv4<E>(
        mdns_query_type: MdnsQueryType,
        packet_ttl: Option<u32>,
        ipv4_if: Option<Ipv4Addr>,
        loop_handle: &Handle,
    ) -> (
        Box<Future<Item = MdnsClientStream, Error = io::Error>>,
        Box<DnsStreamHandle<Error = E>>,
    )
    where
        E: FromProtoError + 'static,
    {
        Self::new::<E>(
            *MDNS_IPV4,
            mdns_query_type,
            packet_ttl,
            ipv4_if,
            None,
            loop_handle,
        )
    }

    /// associates the socket to the well-known ipv6 multicast addess
    pub fn new_ipv6<E>(
        mdns_query_type: MdnsQueryType,
        packet_ttl: Option<u32>,
        ipv6_if: Option<u32>,
        loop_handle: &Handle,
    ) -> (
        Box<Future<Item = MdnsClientStream, Error = io::Error>>,
        Box<DnsStreamHandle<Error = E>>,
    )
    where
        E: FromProtoError + 'static,
    {
        Self::new::<E>(
            *MDNS_IPV6,
            mdns_query_type,
            packet_ttl,
            None,
            ipv6_if,
            loop_handle,
        )
    }

    /// it is expected that the resolver wrapper will be responsible for creating and managing
    ///  new UdpClients such that each new client would have a random port (reduce chance of cache
    ///  poisoning)
    ///
    /// # Return
    ///
    /// a tuple of a Future Stream which will handle sending and receiving messsages, and a
    ///  handle which can be used to send messages into the stream.
    pub fn new<E>(
        mdns_addr: SocketAddr,
        mdns_query_type: MdnsQueryType,
        packet_ttl: Option<u32>,
        ipv4_if: Option<Ipv4Addr>,
        ipv6_if: Option<u32>,
        loop_handle: &Handle,
    ) -> (
        Box<Future<Item = MdnsClientStream, Error = io::Error>>,
        Box<DnsStreamHandle<Error = E>>,
    )
    where
        E: FromProtoError + 'static,
    {
        let (stream_future, sender) = MdnsStream::new(mdns_addr, mdns_query_type, packet_ttl, ipv4_if, ipv6_if, loop_handle);

        let new_future: Box<Future<Item = MdnsClientStream, Error = io::Error>> =
            Box::new(stream_future.map(move |mdns_stream| {
                MdnsClientStream {
                    mdns_stream: mdns_stream,
                }
            }));

        let sender = Box::new(BufDnsStreamHandle {
            name_server: mdns_addr,
            sender: sender,
        });

        (new_future, sender)
    }
}

impl Stream for MdnsClientStream {
    type Item = Vec<u8>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.mdns_stream.poll()) {
            Some((buffer, _src_addr)) => {
                // TODO: for mDNS queries could come from anywhere. It's not clear that there is anything
                //       we can validate in this case.
                Ok(Async::Ready(Some(buffer)))
            }
            None => Ok(Async::Ready(None)),
        }
    }
}
