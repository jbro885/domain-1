//! Looking up host names.

use crate::base::iana::Rtype;
use crate::base::message::RecordIter;
use crate::base::name::{ParsedDname, ToDname, ToRelativeDname};
use crate::base::octets::OctetsRef;
use crate::rdata::{Aaaa, A};
use crate::resolv::resolver::{Resolver, SearchNames};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

//------------ lookup_host ---------------------------------------------------

/// Creates a future that resolves a host name into its IP addresses.
///
/// The future will use the resolver given in `resolv` to query the
/// DNS for the IPv4 and IPv6 addresses associated with `name`.
///
/// The value returned upon success can be turned into an iterator over
/// IP addresses or even socket addresses. Since the lookup may determine that
/// the host name is in fact an alias for another name, the value will also
/// return the canonical name.
pub async fn lookup_host<R: Resolver>(
    resolver: &R,
    qname: impl ToDname,
) -> Result<FoundHosts<R>, io::Error> {
    let (a, aaaa) = tokio::join!(
        resolver.query((&qname, Rtype::A)),
        resolver.query((&qname, Rtype::Aaaa)),
    );
    FoundHosts::new(aaaa, a)
}

//------------ search_host ---------------------------------------------------

pub async fn search_host<R: Resolver + SearchNames>(
    resolver: &R,
    qname: impl ToRelativeDname,
) -> Result<FoundHosts<R>, io::Error> {
    for suffix in resolver.search_iter() {
        if let Ok(name) = (&qname).chain(suffix) {
            if let Ok(answer) = lookup_host(resolver, name).await {
                if !answer.is_empty() {
                    return Ok(answer);
                }
            }
        }
    }
    lookup_host(resolver, qname.chain_root()).await
}

//------------ FoundHosts ----------------------------------------------------

/// The value returned by a successful host lookup.
///
/// You can use the `iter()` method to get an iterator over the IP addresses
/// or `port_iter()` to get an iterator over socket addresses with the given
/// port.
///
/// The `canonical_name()` method returns the canonical name of the host for
/// which the addresses were found.
#[derive(Debug)]
pub struct FoundHosts<R: Resolver> {
    /// The answer to the AAAA query.
    aaaa: Result<R::Answer, io::Error>,

    /// The answer to the A query.
    a: Result<R::Answer, io::Error>,
}

impl<R: Resolver> FoundHosts<R> {
    pub fn new(
        aaaa: Result<R::Answer, io::Error>,
        a: Result<R::Answer, io::Error>,
    ) -> Result<Self, io::Error> {
        if aaaa.is_err() && a.is_err() {
            match aaaa {
                Err(err) => return Err(err),
                _ => unreachable!(),
            }
        }
        Ok(FoundHosts { aaaa, a })
    }

    pub fn is_empty(&self) -> bool {
        if let Ok(ref aaaa) = self.aaaa {
            if aaaa.as_ref().header_counts().ancount() > 0 {
                return false;
            }
        }
        if let Ok(ref a) = self.a {
            if a.as_ref().header_counts().ancount() > 0 {
                return false;
            }
        }
        true
    }

    /// Returns a reference to one of the answers.
    fn answer(&self) -> &R::Answer {
        match self.aaaa.as_ref() {
            Ok(answer) => answer,
            Err(_) => self.a.as_ref().unwrap(),
        }
    }
}

impl<R: Resolver> FoundHosts<R>
where
    for<'a> &'a R::Octets: OctetsRef,
{
    pub fn qname(&self) -> ParsedDname<&R::Octets> {
        self.answer()
            .as_ref()
            .first_question()
            .unwrap()
            .into_qname()
    }

    /// Returns a reference to the canonical name for the host.
    pub fn canonical_name(&self) -> ParsedDname<&R::Octets> {
        self.answer().as_ref().canonical_name().unwrap()
    }

    /// Returns an iterator over the IP addresses returned by the lookup.
    pub fn iter(&self) -> FoundHostsIter<&R::Octets> {
        FoundHostsIter {
            name: self.canonical_name(),
            aaaa: {
                self.aaaa
                    .as_ref()
                    .ok()
                    .and_then(|msg| msg.as_ref().answer().ok())
                    .map(|answer| answer.limit_to::<Aaaa>())
            },
            a: {
                self.a
                    .as_ref()
                    .ok()
                    .and_then(|msg| msg.as_ref().answer().ok())
                    .map(|answer| answer.limit_to::<A>())
            },
        }
    }

    /// Returns an iterator over socket addresses gained from the lookup.
    ///
    /// The socket addresses are gained by combining the IP addresses with
    /// `port`. The returned iterator implements `ToSocketAddrs` and thus
    /// can be used where `std::net` wants addresses right away.
    pub fn port_iter(&self, port: u16) -> FoundHostsSocketIter<&R::Octets> {
        FoundHostsSocketIter {
            iter: self.iter(),
            port,
        }
    }
}

//------------ FoundHostsIter ------------------------------------------------

/// An iterator over the IP addresses returned by a host lookup.
#[derive(Clone, Debug)]
pub struct FoundHostsIter<Ref: OctetsRef> {
    name: ParsedDname<Ref>,
    aaaa: Option<RecordIter<Ref, Aaaa>>,
    a: Option<RecordIter<Ref, A>>,
}

impl<Ref: OctetsRef> Iterator for FoundHostsIter<Ref> {
    type Item = IpAddr;

    fn next(&mut self) -> Option<IpAddr> {
        while let Some(res) = self.aaaa.as_mut().and_then(Iterator::next) {
            if let Ok(record) = res {
                if *record.owner() == self.name {
                    return Some(record.data().addr().into());
                }
            }
        }
        while let Some(res) = self.a.as_mut().and_then(Iterator::next) {
            if let Ok(record) = res {
                if *record.owner() == self.name {
                    return Some(record.data().addr().into());
                }
            }
        }
        None
    }
}

//------------ FoundHostsSocketIter ------------------------------------------

/// An iterator over socket addresses derived from a host lookup.
#[derive(Clone, Debug)]
pub struct FoundHostsSocketIter<Ref: OctetsRef> {
    iter: FoundHostsIter<Ref>,
    port: u16,
}

impl<Ref: OctetsRef> Iterator for FoundHostsSocketIter<Ref> {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        self.iter
            .next()
            .map(|addr| SocketAddr::new(addr, self.port))
    }
}

impl<Ref: OctetsRef> ToSocketAddrs for FoundHostsSocketIter<Ref> {
    type Iter = Self;

    fn to_socket_addrs(&self) -> io::Result<Self> {
        Ok(self.clone())
    }
}
