use std::error::Error as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use url::{Host, Url};

const BLOCKED_DESTINATION_MESSAGE: &str = "external proxy destination is not public";

#[derive(Debug, PartialEq, Eq)]
pub enum ExternalTargetError {
    Invalid,
    Blocked,
}

#[derive(Clone, Copy, Debug)]
pub struct PublicResolver;

impl ureq::Resolver for PublicResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let addresses = netloc.to_socket_addrs()?.collect::<Vec<_>>();
        validate_resolved_addresses(addresses)
    }
}

pub fn external_target(raw_url: &str, prefix: &str) -> Result<Url, ExternalTargetError> {
    let target = raw_url
        .strip_prefix(prefix)
        .ok_or(ExternalTargetError::Invalid)?;
    let target = Url::parse(target).map_err(|_| ExternalTargetError::Invalid)?;
    validate_url(&target)?;
    Ok(target)
}

fn validate_url(target: &Url) -> Result<(), ExternalTargetError> {
    if !matches!(target.scheme(), "http" | "https")
        || target.host().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
    {
        return Err(ExternalTargetError::Invalid);
    }

    match target.host().expect("host checked above") {
        Host::Domain(domain) if is_localhost_name(domain) => Err(ExternalTargetError::Blocked),
        Host::Ipv4(address) if !is_public_ipv4(address) => Err(ExternalTargetError::Blocked),
        Host::Ipv6(address) if !is_public_ipv6(address) => Err(ExternalTargetError::Blocked),
        _ => Ok(()),
    }
}

fn is_localhost_name(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost")
}

fn validate_resolved_addresses(addresses: Vec<SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "external proxy destination did not resolve",
        ));
    }

    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(blocked_destination_error());
    }

    Ok(addresses)
}

fn blocked_destination_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, BLOCKED_DESTINATION_MESSAGE)
}

pub fn is_blocked_destination_error(error: &ureq::Transport) -> bool {
    error.kind() == ureq::ErrorKind::Dns
        && error
            .source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some_and(|source| source.kind() == io::ErrorKind::PermissionDenied)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();

    !matches!(
        (first, second, third),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4() {
        return is_public_ipv4(mapped);
    }

    let segments = address.segments();
    let first = segments[0];

    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
        || first & 0xffc0 == 0xfec0
    {
        return false;
    }

    // IETF special-purpose, documentation, benchmarking, and local translation ranges.
    if (first == 0x0100 && segments[1..4] == [0, 0, 0])
        || (first == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
        || (first == 0x2001 && segments[1] <= 0x01ff)
        || (first == 0x2001 && segments[1] == 0x0db8)
        || (first == 0x3fff && segments[1] < 0x1000)
        || first == 0x5f00
    {
        return false;
    }

    // Validate IPv4 destinations embedded in well-known transition prefixes.
    if first == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    if first == 0x2002 {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use ureq::Resolver as _;

    const PREFIX: &str = "/__ext__/";

    #[test]
    fn keeps_public_urls_and_query_strings() {
        let raw = "/__ext__/https://stremio-server.example/hlsv2/probe?mediaURL=magnet%3Ax&audioCodecs=mp3";
        assert_eq!(
            external_target(raw, PREFIX).unwrap().as_str(),
            "https://stremio-server.example/hlsv2/probe?mediaURL=magnet%3Ax&audioCodecs=mp3"
        );
        assert!(external_target("/__ext__/http://1.1.1.1/manifest.json", PREFIX).is_ok());
        assert!(external_target("/__ext__/https://[2606:4700:4700::1111]/", PREFIX).is_ok());
    }

    #[test]
    fn rejects_invalid_targets_and_credentials() {
        for raw in [
            "/scripts/main.js?v=1",
            "/__ext__/",
            "/__ext__/not a url",
            "/__ext__/file:///etc/passwd",
            "/__ext__/https://user@example.com/",
            "/__ext__/https://user:password@example.com/",
        ] {
            assert!(external_target(raw, PREFIX).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn rejects_localhost_names() {
        for raw in [
            "/__ext__/http://localhost/",
            "/__ext__/http://LOCALHOST./",
            "/__ext__/http://service.localhost/",
        ] {
            assert_eq!(
                external_target(raw, PREFIX),
                Err(ExternalTargetError::Blocked),
                "accepted {raw}"
            );
        }
    }

    #[test]
    fn rejects_non_public_ipv4_targets() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "2130706433",
            "0177.0.0.1",
            "0x7f000001",
        ] {
            let raw = format!("/__ext__/http://{address}/");
            assert_eq!(
                external_target(&raw, PREFIX),
                Err(ExternalTargetError::Blocked),
                "accepted {address}"
            );
        }
    }

    #[test]
    fn rejects_non_public_ipv6_targets() {
        for address in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "2001:db8::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            let raw = format!("/__ext__/http://[{address}]/");
            assert_eq!(
                external_target(&raw, PREFIX),
                Err(ExternalTargetError::Blocked),
                "accepted {address}"
            );
        }
    }

    #[test]
    fn resolver_rejects_any_mixed_private_answer() {
        let addresses = vec![
            "93.184.216.34:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ];
        let error = validate_resolved_addresses(addresses).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn resolver_accepts_public_answers() {
        let addresses = vec![
            "93.184.216.34:443".parse().unwrap(),
            "[2606:4700:4700::1111]:443".parse().unwrap(),
        ];
        assert_eq!(
            validate_resolved_addresses(addresses.clone()).unwrap(),
            addresses
        );
    }

    #[test]
    fn redirect_to_private_address_is_rejected_by_connection_resolver() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let resolver = move |netloc: &str| {
            if netloc.starts_with("public.test:") {
                Ok(vec![server_address])
            } else {
                PublicResolver.resolve(netloc)
            }
        };
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .resolver(resolver)
            .build();
        let error = agent
            .get(&format!("http://public.test:{}/", server_address.port()))
            .call()
            .unwrap_err();

        server.join().unwrap();
        let ureq::Error::Transport(error) = error else {
            panic!("expected transport error");
        };
        assert!(is_blocked_destination_error(&error));
    }

    fn read_request(stream: &mut TcpStream) {
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).unwrap();
    }
}
