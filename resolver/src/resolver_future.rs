// Copyright 2015-2017 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Structs for creating and using a ResolverFuture
use std::io;

use tokio_core::reactor::Handle;
use trust_dns::rr::Name;

use config::{ResolverConfig, ResolverOpts};
use name_server_pool::NameServerPool;
use lookup_ip::{InnerLookupIpFuture, LookupIpFuture};
use system_conf;

/// A Resolver for DNS records.
pub struct ResolverFuture {
    options: ResolverOpts,
    pool: NameServerPool,
}

impl ResolverFuture {
    /// Construct a new ResolverFuture with the associated Client.
    pub fn new(config: ResolverConfig, options: ResolverOpts, reactor: &Handle) -> Self {
        let pool = NameServerPool::from_config(&config, &options, reactor);
        ResolverFuture { options, pool }
    }

    /// Constructs a new Resolver with the given ClientConnection, see UdpClientConnection and/or TcpCLientConnection
    ///
    /// This will read the systems `/etc/cresolv.conf`. Only Unix like OSes are currently supported.
    pub fn from_system_conf(reactor: &Handle) -> io::Result<Self> {
        let (config, options) = system_conf::read_system_conf()?;
        Ok(Self::new(config, options, reactor))
    }

    /// Performs a DNS lookup for the IP for the given hostname.
    ///
    /// Based on the configuration and options passed in, this may do either a A or a AAAA lookup,
    ///  returning IpV4 or IpV6 addresses. (*Note*: current release only queries A, IPv4)
    ///
    /// # Arguments
    /// * `host` - string hostname, if this is an invalid hostname, an error will be thrown. Currently this must be a FQDN, with a trailing `.`, e.g. `www.example.com.`. This will be fixed in a future release.
    pub fn lookup_ip(&mut self, host: &str) -> LookupIpFuture {
        // FIXME: check for FQDN...
        let name = match Name::parse(host, None) {
            Ok(name) => name,
            Err(err) => {
                return InnerLookupIpFuture::error(self.pool.clone(), err)
            }
        };

        // TODO: create list of names to lookup, unless FQDN = only query that

        // create the lookup
        LookupIpFuture::lookup(vec![name], self.options.ip_strategy, &mut self.pool)
    }
}



#[cfg(test)]
mod tests {
    extern crate tokio_core;

    use std::net::*;

    use self::tokio_core::reactor::Core;

    use super::*;

    #[test]
    fn test_lookup() {
        let mut io_loop = Core::new().unwrap();
        let mut resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts::default(),
            &io_loop.handle(),
        );

        let response = io_loop.run(resolver.lookup_ip("www.example.com.")).expect(
            "failed to run lookup",
        );

        assert_eq!(response.iter().count(), 2);
        for address in response {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert_eq!(
                    address,
                    IpAddr::V6(Ipv6Addr::new(
                        0x2606,
                        0x2800,
                        0x220,
                        0x1,
                        0x248,
                        0x1893,
                        0x25c8,
                        0x1946,
                    ))
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn test_system_lookup() {
        let mut io_loop = Core::new().unwrap();
        let mut resolver = ResolverFuture::from_system_conf(&io_loop.handle()).unwrap();

        let response = io_loop.run(resolver.lookup_ip("www.example.com.")).expect(
            "failed to run lookup",
        );

        assert_eq!(response.iter().count(), 2);
        for address in response {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert_eq!(
                    address,
                    IpAddr::V6(Ipv6Addr::new(
                        0x2606,
                        0x2800,
                        0x220,
                        0x1,
                        0x248,
                        0x1893,
                        0x25c8,
                        0x1946,
                    ))
                );
            }
        }
    }
}