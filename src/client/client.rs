// Copyright (C) 2015 - 2016 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use openssl::crypto::pkey::Role;

use ::error::*;
use ::rr::{DNSClass, RecordType, Record, RData};
use ::rr::domain;
use ::rr::dnssec::{Signer, TrustAnchor};
use ::op::{ Message, MessageType, OpCode, Query, Edns, ResponseCode };
use ::serialize::binary::*;
use ::client::ClientConnection;

/// The Client is abstracted over either trust_dns::tcp::TcpClientConnection or
///  trust_dns::udp::UdpClientConnection, usage of TCP or UDP is up to the user. Some DNS servers
///  disallow TCP in some cases, so if TCP double check if UDP works.
pub struct Client<C: ClientConnection> {
  client_connection: RefCell<C>,
  next_id: Cell<u16>,
}

impl<C: ClientConnection> Client<C> {
  /// name_server to connect to with default port 53
  pub fn new(client_connection: C) -> Client<C> {
    Client{ client_connection: RefCell::new(client_connection), next_id: Cell::new(1037) }
  }

  /// When the resolver receives an answer via the normal DNS lookup process, it then checks to
  ///  make sure that the answer is correct. Then starts
  ///  with verifying the DS and DNSKEY records at the DNS root. Then use the DS
  ///  records for the top level domain found at the root, e.g. 'com', to verify the DNSKEY
  ///  records in the 'com' zone. From there see if there is a DS record for the
  ///  subdomain, e.g. 'example.com', in the 'com' zone, and if there is use the
  ///  DS record to verify a DNSKEY record found in the 'example.com' zone. Finally,
  ///  verify the RRSIG record found in the answer for the rrset, e.g. 'www.example.com'.
  pub fn secure_query(&self, name: &domain::Name, query_class: DNSClass, query_type: RecordType) -> ClientResult<Message> {
    // TODO: if we knew we were talking with a DNS server that supported multiple queries, these
    //  could be a single multiple query request...

    // with the secure setting, we should get the RRSIG as well as the answer
    //  the RRSIG is signed by the DNSKEY, the DNSKEY is signed by the DS record in the Parent
    //  zone. The key_tag is the DS record is assigned to the DNSKEY.
    let record_response = try!(self.inner_query(name, query_class, query_type, true));
    {
      let rrsigs: Vec<&Record> = record_response.get_answers().iter().filter(|rr| rr.get_rr_type() == RecordType::RRSIG).collect();

      if rrsigs.is_empty() {
        return Err(ClientError::NoRRSIG);
      }

      // group the record sets by name and type
      let mut rrset_types: HashSet<(domain::Name, RecordType)> = HashSet::new();
      for rrset in record_response.get_answers().iter()
                                  .filter(|rr| rr.get_rr_type() != RecordType::RRSIG)
                                  .map(|rr| (rr.get_name().clone(), rr.get_rr_type())) {
        rrset_types.insert(rrset);
      }

      // verify all returned rrsets
      for (name, rrset_type) in rrset_types {
        let rrset: Vec<&Record> = record_response.get_answers().iter().filter(|rr| rr.get_rr_type() == rrset_type && rr.get_name() == &name).collect();

        // '. DNSKEY' -> 'com. DS' -> 'com. DNSKEY' -> 'examle.com. DS' -> 'example.com. DNSKEY'
        // 'com. DS' is signed by '. DNSKEY' which produces 'com. RRSIG', all are in the same zone, '.'
        //  the '.' DNSKEY is signed by the well known root certificate.
        // TODO fix rrsigs clone()
        let proof = try!(self.recursive_query_verify(&name, rrset, rrsigs.clone(), query_type, query_class));

        // TODO return this, also make a prettier print
        debug!("proved existance through: {:?}", proof);
      }
    }

    // getting here means that we looped through all records with validation
    Ok(record_response)
  }

  /// Verifies a record set against the supplied signatures, looking up the DNSKey chain.
  /// returns the chain of proof or an error if there is none.
  fn recursive_query_verify(&self, name: &domain::Name, rrset: Vec<&Record>, rrsigs: Vec<&Record>,
    query_type: RecordType, query_class: DNSClass) -> ClientResult<Vec<Record>> {

    // TODO: this is ugly, what reference do I want?
    let rrset: Vec<Record> = rrset.iter().map(|rr|rr.clone()).cloned().collect();

    // verify the DNSKey via a DS key if it's the secure_entry_point
    if let Some(record) = rrset.first() {
      if record.get_rr_type() == RecordType::DNSKEY {
        if let &RData::DNSKEY{zone_key, secure_entry_point, ..} = record.get_rdata() {
          // the spec says that the secure_entry_point isn't reliable for the main DNSKey...
          //  but how do you know which needs to be validated with the DS in the parent zone?
          if zone_key && secure_entry_point {
            let mut proof = try!(self.verify_dnskey(record));
            // TODO: this is verified, it can be cached
            proof.push(record.clone());
            return Ok(proof);
          }
        } else {
          panic!("expected DNSKEY");
        }
      }
    }

    // standard rrsig verification
    for rrsig in rrsigs.iter().filter(|rr| rr.get_name() == name) {
      if let &RData::SIG{ref sig, ref signer_name, algorithm: sig_alg, ..} = rrsig.get_rdata() {
        // get DNSKEY from signer_name
        let key_response = try!(self.inner_query(&signer_name, query_class, RecordType::DNSKEY, true));
        let key_rrset: Vec<&Record> = key_response.get_answers().iter().filter(|rr| rr.get_rr_type() == RecordType::DNSKEY).collect();
        let key_rrsigs: Vec<&Record> = key_response.get_answers().iter().filter(|rr| rr.get_rr_type() == RecordType::RRSIG).collect();

        for dnskey in key_rrset.iter() {
          if let &RData::DNSKEY{zone_key, algorithm, revoke, ref public_key, ..} = dnskey.get_rdata() {
            if revoke { debug!("revoked: {}", dnskey.get_name()); continue } // TODO: does this need to be validated? RFC 5011
            if !zone_key { continue }
            if algorithm != sig_alg { continue }

            let pkey = try!(algorithm.public_key_from_vec(public_key));
            if !pkey.can(Role::Verify) { debug!("pkey can't verify, {:?}", dnskey.get_name()); continue }

            let signer: Signer = Signer::new(algorithm, pkey, signer_name.clone());
            let rrset_hash: Vec<u8> = signer.hash_rrset(rrsig, &rrset);

            if signer.verify(&rrset_hash, sig) {
              if signer_name == name && query_type == RecordType::DNSKEY {
                // this is self signed... let's skip to DS validation
                let mut proof: Vec<Record> = try!(self.verify_dnskey(dnskey));
                // TODO: this is verified, cache it
                proof.push((*dnskey).clone());
                return Ok(proof);
              } else {
                let mut proof = try!(self.recursive_query_verify(&signer_name, key_rrset.clone(), key_rrsigs, RecordType::DNSKEY, query_class));
                // TODO: this is verified, cache it
                proof.push((*dnskey).clone());
                return Ok(proof);
              }
            } else {
              debug!("could not verify: {} with: {}", name, rrsig.get_name());
            }
          } else {
            panic!("this should be a DNSKEY")
          }
        }
      } else {
        panic!("expected RRSIG: {:?}", rrsig.get_rr_type());
      }
    }

    Err(ClientError::NoRRSIG)
  }

  /// attempts to verify the DNSKey against the DS of the parent.
  /// returns the chain of proof or an error if there is none.
  fn verify_dnskey(&self, dnskey: &Record) -> ClientResult<Vec<Record>> {
    let name: &domain::Name = dnskey.get_name();

    if dnskey.get_name().is_root() {
      if let &RData::DNSKEY{ ref public_key, .. } = dnskey.get_rdata() {
        if TrustAnchor::new().contains(public_key) {
          return Ok(vec![dnskey.clone()])
        }
      }
    }

    let ds_response = try!(self.inner_query(&name, dnskey.get_dns_class(), RecordType::DS, true));
    let ds_rrset: Vec<&Record> = ds_response.get_answers().iter().filter(|rr| rr.get_rr_type() == RecordType::DS).collect();
    let ds_rrsigs: Vec<&Record> = ds_response.get_answers().iter().filter(|rr| rr.get_rr_type() == RecordType::RRSIG).collect();

    for ds in ds_rrset.iter() {
      if let &RData::DS{digest_type, ref digest, ..} = ds.get_rdata() {
        // 5.1.4.  The Digest Field
        //
        //    The DS record refers to a DNSKEY RR by including a digest of that
        //    DNSKEY RR.
        //
        //    The digest is calculated by concatenating the canonical form of the
        //    fully qualified owner name of the DNSKEY RR with the DNSKEY RDATA,
        //    and then applying the digest algorithm.
        //
        //      digest = digest_algorithm( DNSKEY owner name | DNSKEY RDATA);
        //
        //       "|" denotes concatenation
        //
        //      DNSKEY RDATA = Flags | Protocol | Algorithm | Public Key.
        //
        //    The size of the digest may vary depending on the digest algorithm and
        //    DNSKEY RR size.  As of the time of this writing, the only defined
        //    digest algorithm is SHA-1, which produces a 20 octet digest.
        let mut buf: Vec<u8> = Vec::new();
        {
          let mut encoder: BinEncoder = BinEncoder::new(&mut buf);
          encoder.set_canonical_names(true);
          try!(name.emit(&mut encoder));
          try!(dnskey.get_rdata().emit(&mut encoder));
        }

        let hash: Vec<u8> = digest_type.hash(&buf);
        if &hash == digest {
          // continue to verify the chain...
          let mut proof: Vec<Record> = try!(self.recursive_query_verify(&name, ds_rrset.clone(), ds_rrsigs, RecordType::DNSKEY, dnskey.get_dns_class()));
          proof.push(dnskey.clone());
          return Ok(proof)
        }
      } else {
        panic!("expected DS: {:?}", ds.get_rr_type());
      }
    }

    Err(ClientError::NoDS)
  }


  // send a DNS query to the name_server specified in Clint.
  //
  // ```
  // use std::net::*;
  //
  // use trust_dns::rr::dns_class::DNSClass;
  // use trust_dns::rr::record_type::RecordType;
  // use trust_dns::rr::domain;
  // use trust_dns::rr::record_data::RData;
  // use trust_dns::udp::client::Client;
  //
  // let name = domain::Name::with_labels(vec!["www".to_string(), "example".to_string(), "com".to_string()]);
  // let client = Client::new(("8.8.8.8").parse().unwrap()).unwrap();
  // let response = client.query(name.clone(), DNSClass::IN, RecordType::A).unwrap();
  //
  // let record = &response.get_answers()[0];
  // assert_eq!(record.get_name(), &name);
  // assert_eq!(record.get_rr_type(), RecordType::A);
  // assert_eq!(record.get_dns_class(), DNSClass::IN);
  //
  // if let &RData::A{ ref address } = record.get_rdata() {
  //   assert_eq!(address, &Ipv4Addr::new(93,184,216,34))
  // } else {
  //   assert!(false);
  // }
  //
  // ```
  pub fn query(&self, name: &domain::Name, query_class: DNSClass, query_type: RecordType) -> ClientResult<Message> {
    self.inner_query(name, query_class, query_type, false)
  }

  fn inner_query(&self, name: &domain::Name, query_class: DNSClass, query_type: RecordType, secure: bool) -> ClientResult<Message> {
    debug!("querying: {} {:?}", name, query_type);

    // TODO: this isn't DRY, duplicate code with the TCP client

    // build the message
    let mut message: Message = Message::new();
    let id = self.next_id();
    // TODO make recursion a parameter
    message.id(id).message_type(MessageType::Query).op_code(OpCode::Query).recursion_desired(true);

    // Extended dns
    let mut edns: Edns = Edns::new();

    if secure {
      edns.set_dnssec_ok(true);
      message.authentic_data(true);
      message.checking_disabled(false);
    }

    edns.set_max_payload(1500);
    edns.set_version(0);

    message.set_edns(edns);

    // add the query
    let mut query: Query = Query::new();
    query.name(name.clone()).query_class(query_class).query_type(query_type);
    message.add_query(query);

    // get the message bytes and send the query
    let mut buffer: Vec<u8> = Vec::with_capacity(512);
    {
      let mut encoder = BinEncoder::new(&mut buffer);
      try!(message.emit(&mut encoder));
    }

    // send the message and get the response from the connection.
    let resp_buffer = try!(self.client_connection.borrow_mut().send(&buffer));

    let mut decoder = BinDecoder::new(&resp_buffer);
    let response = try!(Message::read(&mut decoder));

    if response.get_id() != id { return Err(ClientError::IncorrectMessageId{ got: response.get_id(), expect: id }); }
    if response.get_response_code() != ResponseCode::NoError { return Err(ClientError::ErrorResponse(response.get_response_code())); }

    Ok(response)
  }

  fn next_id(&self) -> u16 {
    let id = self.next_id.get();
    self.next_id.set(id + 1);
    id
  }
}

#[cfg(test)]
mod test {
  use std::net::*;

  use ::rr::dns_class::DNSClass;
  use ::rr::record_type::RecordType;
  use ::rr::domain;
  use ::rr::record_data::RData;
  use ::udp::UdpClientConnection;
  use ::tcp::TcpClientConnection;
  use super::Client;
  use super::super::ClientConnection;

  #[test]
  #[cfg(feature = "ftest")]
  fn test_query_udp() {
    let addr: SocketAddr = ("8.8.8.8",53).to_socket_addrs().unwrap().next().unwrap();
    let conn = UdpClientConnection::new(addr).unwrap();
    test_query(conn);
  }

  #[test]
  #[cfg(feature = "ftest")]
  fn test_query_tcp() {
    let addr: SocketAddr = ("8.8.8.8",53).to_socket_addrs().unwrap().next().unwrap();
    let conn = TcpClientConnection::new(addr).unwrap();
    test_query(conn);
  }


  // TODO: this should be flagged with cfg as a functional test.
  #[cfg(test)]
  fn test_query<C: ClientConnection>(conn: C) {
    let name = domain::Name::with_labels(vec!["www".to_string(), "example".to_string(), "com".to_string()]);
    let client = Client::new(conn);

    let response = client.query(&name, DNSClass::IN, RecordType::A);
    assert!(response.is_ok(), "query failed: {}", response.unwrap_err());

    let response = response.unwrap();

    println!("response records: {:?}", response);

    let record = &response.get_answers()[0];
    assert_eq!(record.get_name(), &name);
    assert_eq!(record.get_rr_type(), RecordType::A);
    assert_eq!(record.get_dns_class(), DNSClass::IN);

    if let &RData::A{ ref address } = record.get_rdata() {
      assert_eq!(address, &Ipv4Addr::new(93,184,216,34))
    } else {
      assert!(false);
    }
  }

  #[test]
  #[cfg(feature = "ftest")]
  fn test_secure_query_example_udp() {
    let addr: SocketAddr = ("8.8.8.8",53).to_socket_addrs().unwrap().next().unwrap();
    let conn = UdpClientConnection::new(addr).unwrap();
    test_secure_query_example(conn);
  }

  #[test]
  #[cfg(feature = "ftest")]
  fn test_secure_query_example_tcp() {
    let addr: SocketAddr = ("8.8.8.8",53).to_socket_addrs().unwrap().next().unwrap();
    let conn = TcpClientConnection::new(addr).unwrap();
    test_secure_query_example(conn);
  }

  #[cfg(test)]
  fn test_secure_query_example<C: ClientConnection>(conn: C) {
    let name = domain::Name::with_labels(vec!["www".to_string(), "example".to_string(), "com".to_string()]);
    let client = Client::new(conn);

    let response = client.secure_query(&name, DNSClass::IN, RecordType::A);
    assert!(response.is_ok(), "query failed: {}", response.unwrap_err());

    let response = response.unwrap();

    println!("response records: {:?}", response);

    let record = &response.get_answers()[0];
    assert_eq!(record.get_name(), &name);
    assert_eq!(record.get_rr_type(), RecordType::A);
    assert_eq!(record.get_dns_class(), DNSClass::IN);

    if let &RData::A{ ref address } = record.get_rdata() {
      assert_eq!(address, &Ipv4Addr::new(93,184,216,34))
    } else {
      assert!(false);
    }
  }

  // // TODO: use this site for verifying nsec3
  // #[test]
  // #[cfg(feature = "ftest")]
  // fn test_secure_query_sdsmt() {
  //   use std::net::*;
  //
  //   use ::rr::dns_class::DNSClass;
  //   use ::rr::record_type::RecordType;
  //   use ::rr::domain;
  //   use ::rr::record_data::RData;
  //   use ::udp::Client;
  //
  //   let name = domain::Name::with_labels(vec!["www".to_string(), "sdsmt".to_string(), "edu".to_string()]);
  //   let client = Client::new(("8.8.8.8").parse().unwrap()).unwrap();
  //
  //   let response = client.secure_query(&name, DNSClass::IN, RecordType::A);
  //   assert!(response.is_ok(), "query failed: {}", response.unwrap_err());
  //
  //   let response = response.unwrap();
  //
  //   println!("response records: {:?}", response);
  //
  //   let record = &response.get_answers()[0];
  //   assert_eq!(record.get_name(), &name);
  //   assert_eq!(record.get_rr_type(), RecordType::A);
  //   assert_eq!(record.get_dns_class(), DNSClass::IN);
  //
  //   if let &RData::A{ ref address } = record.get_rdata() {
  //     assert_eq!(address, &Ipv4Addr::new(93,184,216,34))
  //   } else {
  //     assert!(false);
  //   }
  // }
}