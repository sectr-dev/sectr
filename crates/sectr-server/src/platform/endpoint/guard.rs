//! The address guard for requests to customer endpoints.
//!
//! The server sends the registration probe, the manifest request and every
//! wake from inside our own network, where the cloud metadata service
//! (`169.254.169.254`, which hands out the server's credentials) and local
//! services such as Postgres are reachable. An endpoint URL is customer
//! input, so with the guard on, the server refuses any request that would
//! reach an address set aside for non-public use.
//!
//! The blocked ranges are the ones internet standards reserve (RFC 1122,
//! 1918, 3927, 4193, 4291, 6598, 8215), not a list of our own
//! infrastructure: no public service can live on them, so blocking them
//! never blocks a real endpoint, and the list needs no upkeep. An IPv6
//! address that carries an IPv4 one (mapped, NAT64, 6to4, Teredo) is
//! checked as that IPv4 address.
//!
//! The check runs at send time, in the client's DNS resolver, so a DNS
//! change can't point an accepted hostname at an internal address later.
//! Hosts that are IP literals skip the resolver (hyper-util parses them
//! directly), so [`check_url`] checks those before every request.
//!
//! With the guard off (`SECTR_ENDPOINT_ALLOW_PRIVATE`, for self-hosters
//! reaching services on their own network) only the client's fixed
//! settings apply: no redirects and no proxy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use sectr_common::endpoint::PRIVATE_ADDRESS;

/// Whether an address is one the guard refuses.
pub fn is_blocked(ip: IpAddr) -> bool {
  match ip {
    IpAddr::V4(v4) => is_blocked_v4(v4),
    IpAddr::V6(v6) => match embedded_ipv4(v6) {
      Some(v4) => is_blocked_v4(v4),
      None => is_blocked_v6(v6),
    },
  }
}

/// The IPv4 address an IPv6 address carries, for the forms a host or its
/// network can turn back into IPv4 traffic to that address: on a network
/// with NAT64, `64:ff9b::a9fe:a9fe` reaches `169.254.169.254`.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
  let v4 = |hi: u16, lo: u16| Some(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
  // IPv4-mapped `::ffff:a.b.c.d` and the older IPv4-compatible
  // `::a.b.c.d` (RFC 4291), which includes `::` and `::1` as 0.0.0.0/8.
  if let Some(mapped) = ip.to_ipv4() {
    return Some(mapped);
  }
  match ip.segments() {
    // NAT64's well-known prefix, 64:ff9b::/96 (RFC 6052).
    [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => v4(hi, lo),
    // 6to4, 2002::/16 (RFC 3056): the IPv4 address follows the prefix.
    [0x2002, hi, lo, ..] => v4(hi, lo),
    // Teredo, 2001::/32 (RFC 4380): the client's address, bits inverted.
    [0x2001, 0, _, _, _, _, hi, lo] => v4(!hi, !lo),
    _ => None,
  }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
  let [a, b, c, _] = ip.octets();
  a == 0 // 0.0.0.0/8, "this network" (RFC 1122)
    || ip.is_private() // 10/8, 172.16/12, 192.168/16 (RFC 1918)
    || (a == 100 && (b & 0xc0) == 64) // 100.64/10, carrier-grade NAT (RFC 6598)
    || ip.is_loopback() // 127/8
    || ip.is_link_local() // 169.254/16, including cloud metadata (RFC 3927)
    || (a == 192 && b == 0 && c == 0) // 192.0.0/24, protocol assignments
    || (a == 198 && (b & 0xfe) == 18) // 198.18/15, benchmarking
    || a >= 224 // 224/4 multicast, 240/4 reserved, and broadcast
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
  let [first, second, third, ..] = ip.segments();
  ip.is_unspecified() // ::
    || ip.is_loopback() // ::1
    || (first == 0x64 && second == 0xff9b && third == 1) // 64:ff9b:1::/48, local-use NAT64 (RFC 8215)
    || (first & 0xfe00) == 0xfc00 // fc00::/7, unique local (RFC 4193)
    || (first & 0xffc0) == 0xfe80 // fe80::/10, link local (RFC 4291)
    || (first & 0xff00) == 0xff00 // ff00::/8, multicast
}

/// The rules for an endpoint URL, checked at registration and before every
/// request. With the guard off, only the scheme (http or https), the
/// host's presence and the absence of a query are checked.
pub fn check_url(url: &Url, allow_private: bool) -> Result<(), String> {
  let host = url
    .host_str()
    .filter(|host| !host.is_empty())
    .ok_or_else(|| "the endpoint URL has no host".to_owned())?;
  // Not a guard rule: the wake and manifest requests go to paths under
  // the URL and couldn't carry a query, so an endpoint that needs one
  // would pass the probe and then fail every request.
  if url.query().is_some() {
    return Err(
      "the endpoint URL must not have a ?query: the server sends requests to paths under it \
       (/wake, /manifest) without one"
        .into(),
    );
  }
  if allow_private {
    return match url.scheme() {
      "https" | "http" => Ok(()),
      other => Err(format!("the endpoint URL must be https, not {other}")),
    };
  }
  if url.scheme() != "https" {
    return Err(format!(
      "the endpoint URL must be https, not {}",
      url.scheme()
    ));
  }
  if !url.username().is_empty() || url.password().is_some() {
    return Err("the endpoint URL must not carry a username or password".into());
  }
  if url.fragment().is_some() {
    return Err("the endpoint URL must not have a #fragment".into());
  }
  // IPv6 literals come back bracketed (`[::1]`).
  let literal = host.trim_start_matches('[').trim_end_matches(']');
  let blocked = match literal.parse::<IpAddr>() {
    Ok(ip) => is_blocked(ip),
    Err(_) => {
      let domain = host.trim_end_matches('.').to_ascii_lowercase();
      domain == "localhost" || domain.ends_with(".localhost")
    }
  };
  if blocked {
    return Err(format!(
      "{host} {PRIVATE_ADDRESS}, which this server doesn't allow"
    ));
  }
  Ok(())
}

/// Resolves through the system resolver and FAILS when any resolved
/// address is blocked, rather than quietly dropping it, which would hide a
/// misconfiguration.
#[derive(Debug, Default)]
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
  fn resolve(&self, name: Name) -> Resolving {
    let host = name.as_str().to_owned();
    Box::pin(async move {
      let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
      check_resolved(&host, &addrs)?;
      let addrs: Addrs = Box::new(addrs.into_iter());
      Ok(addrs)
    })
  }
}

/// The resolver's check, separate so tests can feed it addresses.
fn check_resolved(host: &str, addrs: &[SocketAddr]) -> Result<(), BlockedHost> {
  if addrs.iter().any(|addr| is_blocked(addr.ip())) {
    return Err(BlockedHost(host.to_owned()));
  }
  Ok(())
}

/// The error the resolver fails with: [`is_blocked_error`] finds it in a
/// reqwest error's source chain.
#[derive(Debug, thiserror::Error)]
#[error("{0} {PRIVATE_ADDRESS}, which this server doesn't allow")]
pub struct BlockedHost(pub String);

/// Whether a request failed because the guard refused the host.
pub fn is_blocked_error(error: &(dyn std::error::Error + 'static)) -> bool {
  let mut source = Some(error);
  while let Some(e) = source {
    if e.is::<BlockedHost>() {
      return true;
    }
    source = e.source();
  }
  false
}

/// The one client for every request to a customer endpoint: never follows
/// a redirect (it could point anywhere), never uses a proxy (the guard
/// must see the real destination), and with the guard on resolves through
/// [`GuardedResolver`]. Requests set their own timeouts.
pub fn client(allow_private: bool) -> reqwest::Client {
  let mut builder = reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .no_proxy()
    .user_agent(concat!("sectr-server/", env!("CARGO_PKG_VERSION")));
  if !allow_private {
    builder = builder.dns_resolver(Arc::new(GuardedResolver));
  }
  builder.build().expect("the endpoint client builds")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn v4(s: &str) -> bool {
    is_blocked(s.parse().unwrap())
  }

  #[test]
  fn blocked_ipv4_ranges_and_their_edges() {
    for blocked in [
      "0.0.0.0",
      "0.255.255.255",
      "10.0.0.0",
      "10.255.255.255",
      "100.64.0.0",
      "100.127.255.255",
      "127.0.0.1",
      "127.255.255.255",
      "169.254.0.0",
      "169.254.169.254",
      "169.254.255.255",
      "172.16.0.0",
      "172.31.255.255",
      "192.0.0.0",
      "192.0.0.255",
      "192.168.0.0",
      "192.168.255.255",
      "198.18.0.0",
      "198.19.255.255",
      "224.0.0.0",
      "239.255.255.255",
      "240.0.0.1",
      "255.255.255.255",
    ] {
      assert!(v4(blocked), "{blocked} should be blocked");
    }
    for public in [
      "1.0.0.0",
      "9.255.255.255",
      "11.0.0.0",
      "100.63.255.255",
      "100.128.0.0",
      "126.255.255.255",
      "128.0.0.0",
      "169.253.255.255",
      "169.255.0.0",
      "172.15.255.255",
      "172.32.0.0",
      "191.255.255.255",
      "192.0.1.0",
      "192.0.2.1",
      "192.167.255.255",
      "192.169.0.0",
      "198.17.255.255",
      "198.20.0.0",
      "223.255.255.255",
      "8.8.8.8",
      "34.117.59.81",
    ] {
      assert!(!v4(public), "{public} should be allowed");
    }
  }

  #[test]
  fn blocked_ipv6_ranges_and_their_edges() {
    for blocked in [
      "::",
      "::1",
      "fc00::",
      "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
      "fe80::",
      "febf:ffff::1",
      "ff00::",
      "ff02::1",
      "64:ff9b:1::",
      "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
    ] {
      assert!(v4(blocked), "{blocked} should be blocked");
    }
    for public in [
      "::1:0:0:0",
      "64:ff9b:2::1",
      "fbff:ffff::1",
      "fe00::",
      "fe7f:ffff::1",
      "fec0::",
      "feff:ffff::1",
      "2001:4860:4860::8888",
      "2606:4700::1111",
    ] {
      assert!(!v4(public), "{public} should be allowed");
    }
  }

  #[test]
  fn ipv6_addresses_that_carry_ipv4_are_checked_as_ipv4() {
    for (blocked, public) in [
      // IPv4-mapped.
      ("::ffff:169.254.169.254", "::ffff:8.8.8.8"),
      ("::ffff:10.0.0.1", "::ffff:34.117.59.81"),
      ("::ffff:127.0.0.1", "::ffff:1.1.1.1"),
      // IPv4-compatible.
      ("::169.254.169.254", "::8.8.8.8"),
      ("::2", "::1.0.0.0"),
      // NAT64.
      ("64:ff9b::a9fe:a9fe", "64:ff9b::808:808"),
      ("64:ff9b::10.0.0.1", "64:ff9b::34.117.59.81"),
      // 6to4.
      ("2002:a9fe:a9fe::", "2002:808:808::"),
      ("2002:7f00:1::1", "2002:2275:3b51::1"),
      // Teredo: the client address 169.254.169.254, bits inverted.
      (
        "2001:0:4136:e378:8000:63bf:5601:5601",
        "2001:0:4136:e378:8000:63bf:f7f7:f7f7",
      ),
    ] {
      assert!(v4(blocked), "{blocked} should be blocked");
      assert!(!v4(public), "{public} should be allowed");
    }
  }

  fn check(url: &str) -> Result<(), String> {
    check_url(&Url::parse(url).unwrap(), false)
  }

  #[test]
  fn registration_rules() {
    assert!(check("https://agent.example.com").is_ok());
    assert!(check("https://abc.lambda-url.us-east-1.on.aws/").is_ok());
    assert!(check("https://8.8.8.8/x").is_ok());

    for (url, reason) in [
      ("https://10.0.0.1/", "private address"),
      ("https://[::ffff:10.0.0.1]/", "private address"),
      ("https://[64:ff9b::a9fe:a9fe]/", "private address"),
      ("https://[fd00::1]/", "private address"),
      ("https://169.254.169.254/", "private address"),
      ("https://localhost/", "private address"),
      ("https://LOCALHOST./", "private address"),
      ("https://api.localhost/", "private address"),
      ("https://user:pw@agent.example.com/", "username or password"),
      ("https://user@agent.example.com/", "username or password"),
      ("https://agent.example.com/#x", "fragment"),
      ("https://agent.example.com/?code=abc", "?query"),
      ("https://agent.example.com/?", "?query"),
      ("http://agent.example.com/", "must be https"),
      ("ftp://agent.example.com/", "must be https"),
    ] {
      let err = check(url).unwrap_err();
      assert!(err.contains(reason), "{url}: {err}");
    }
  }

  #[test]
  fn with_the_guard_off_http_and_private_hosts_are_allowed() {
    for url in [
      "http://127.0.0.1:8080/",
      "http://localhost:9000",
      "https://10.0.0.1",
    ] {
      assert!(check_url(&Url::parse(url).unwrap(), true).is_ok(), "{url}");
    }
    assert!(check_url(&Url::parse("ftp://x/").unwrap(), true).is_err());
    assert!(check_url(&Url::parse("http://localhost:9000/?k=v").unwrap(), true).is_err());
  }

  #[test]
  fn the_resolver_fails_when_any_address_is_private() {
    let public: SocketAddr = "93.184.215.14:0".parse().unwrap();
    let private: SocketAddr = "10.1.2.3:0".parse().unwrap();
    assert!(check_resolved("a.example", &[public]).is_ok());
    let err = check_resolved("a.example", &[public, private]).unwrap_err();
    assert!(
      err
        .to_string()
        .contains("a.example resolves to a private address")
    );
  }

  #[tokio::test]
  async fn the_guarded_client_refuses_a_name_that_resolves_to_loopback() {
    // `localhost` resolves through the system resolver to 127.0.0.1/::1.
    let err = client(false)
      .get("https://localhost:9/")
      .send()
      .await
      .unwrap_err();
    assert!(is_blocked_error(&err), "{err:?}");
  }

  #[tokio::test]
  async fn redirects_are_not_followed() {
    use axum::response::Redirect;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
      "/",
      axum::routing::get(|| async { Redirect::temporary("http://169.254.169.254/") }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = client(true)
      .get(format!("http://{addr}/"))
      .send()
      .await
      .unwrap();
    assert_eq!(response.status().as_u16(), 307);
  }
}
