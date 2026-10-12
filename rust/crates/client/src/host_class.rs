//! What kind of network a hostname names: this device, a private network, a tailnet (port of the three predicates of
//! `packages/shared/src/hostClassification.ts` that route preference uses). Names are classified by their text; nothing is resolved.

fn normalize(host: &str) -> String {
    let lowered = host.to_lowercase();
    let unbracketed = lowered.strip_prefix('[').unwrap_or(&lowered);
    let unbracketed = unbracketed.strip_suffix(']').unwrap_or(unbracketed);
    unbracketed.trim_end_matches('.').to_owned()
}

fn ipv4(host: &str) -> Option<[u32; 4]> {
    let normalized = normalize(host);
    let parts: Vec<&str> = normalized.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut address = [0; 4];
    for (slot, part) in address.iter_mut().zip(parts) {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse::<u32>().ok().filter(|value| *value <= 255)?;
    }
    Some(address)
}

/// `::ffff:a.b.c.d` and `::ffff:hhhh:hhhh`.
fn ipv4_mapped(host: &str) -> Option<[u32; 4]> {
    let normalized = normalize(host);
    let suffix = normalized.strip_prefix("::ffff:")?;
    if let Some(dotted) = ipv4(suffix) {
        return Some(dotted);
    }
    let hextets: Vec<&str> = suffix.split(':').collect();
    if hextets.len() != 2 || hextets.iter().any(|part| part.is_empty() || part.len() > 4 || !part.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return None;
    }
    let high = u32::from_str_radix(hextets[0], 16).ok()?;
    let low = u32::from_str_radix(hextets[1], 16).ok()?;
    Some([high >> 8, high & 0xff, low >> 8, low & 0xff])
}

fn ipv6(host: &str) -> Option<[u32; 8]> {
    let normalized = normalize(host);
    if !normalized.contains(':') {
        return None;
    }
    let halves: Vec<&str> = normalized.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let split = |half: &str| -> Vec<String> { if half.is_empty() { vec![] } else { half.split(':').map(str::to_owned).collect() } };
    let head = split(halves[0]);
    let tail = halves.get(1).map(|half| split(half)).unwrap_or_default();
    if head.iter().chain(&tail).any(|part| part.is_empty() || part.len() > 4 || !part.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return None;
    }
    let missing = 8i32 - head.len() as i32 - tail.len() as i32;
    if (halves.len() == 1 && missing != 0) || (halves.len() == 2 && missing < 1) {
        return None;
    }
    let mut address = [0; 8];
    let parts = head.iter().map(String::as_str).chain(std::iter::repeat_n("0", missing.max(0) as usize)).chain(tail.iter().map(String::as_str));
    for (slot, part) in address.iter_mut().zip(parts) {
        *slot = u32::from_str_radix(part, 16).ok()?;
    }
    Some(address)
}

fn ipv6_prefix_matches(address: &[u32; 8], prefix: &[u32; 8], length: usize) -> bool {
    let full = length / 16;
    if address[..full] != prefix[..full] {
        return false;
    }
    let remaining = length % 16;
    if remaining == 0 {
        return true;
    }
    let mask = (0xffffu32 << (16 - remaining)) & 0xffff;
    address[full] & mask == prefix[full] & mask
}

fn private_ipv4(parts: [u32; 4]) -> bool {
    parts[0] == 0
        || parts[0] == 10
        || parts[0] == 127
        || (parts[0] == 100 && (64..=127).contains(&parts[1]))
        || (parts[0] == 172 && (16..=31).contains(&parts[1]))
        || (parts[0] == 192 && parts[1] == 168)
        || (parts[0] == 169 && parts[1] == 254)
        || (parts[0] == 198 && (18..=19).contains(&parts[1]))
}

/// `isLocalLoopbackHost`.
pub fn is_local_loopback(host: &str) -> bool {
    let normalized = normalize(host);
    normalized == "localhost" || normalized == "::1" || ipv4(&normalized).is_some_and(|parts| parts[0] == 127)
}

/// `isTailnetHost`: a MagicDNS name, the 100.64.0.0/10 range Tailscale assigns, or its IPv6 range fd7a:115c:a1e0::/48.
pub fn is_tailnet(host: &str) -> bool {
    let normalized = normalize(host);
    if normalized.ends_with(".ts.net") {
        return true;
    }
    if let Some(parts) = ipv4(&normalized) {
        return parts[0] == 100 && (64..=127).contains(&parts[1]);
    }
    ipv6(&normalized).is_some_and(|address| ipv6_prefix_matches(&address, &[0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0], 48))
}

/// `isPrivateNetworkHost`.
pub fn is_private_network(host: &str) -> bool {
    let normalized = normalize(host);
    if normalized == "::"
        || is_local_loopback(&normalized)
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
        || normalized == "home.arpa"
        || normalized.ends_with(".home.arpa")
        || (!normalized.contains('.') && !normalized.contains(':'))
    {
        return true;
    }
    if normalized.ends_with(".ts.net") {
        return true;
    }
    if let Some(parts) = ipv4(&normalized).or_else(|| ipv4_mapped(&normalized)) {
        return private_ipv4(parts);
    }
    let first = normalized.split(':').next().unwrap_or_default();
    if !normalized.contains(':') || first.is_empty() || first.len() > 4 || !first.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let hextet = u32::from_str_radix(first, 16).unwrap_or(0);
    hextet & 0xfe00 == 0xfc00 || hextet & 0xffc0 == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_classified_by_their_text() {
        for host in ["localhost", "LOCALHOST.", "127.0.0.1", "127.9.9.9", "[::1]", "::1"] {
            assert!(is_local_loopback(host), "{host}");
        }
        for host in ["example.com", "128.0.0.1", "10.0.0.1"] {
            assert!(!is_local_loopback(host), "{host}");
        }
        for host in ["box.tail1.ts.net", "100.64.0.1", "100.127.255.255", "fd7a:115c:a1e0::1", "[fd7a:115c:a1e0:ab12::7]"] {
            assert!(is_tailnet(host), "{host}");
        }
        for host in ["100.63.0.1", "100.128.0.1", "fd7a:115d::1", "example.com", "192.168.1.1"] {
            assert!(!is_tailnet(host), "{host}");
        }
        for host in ["192.168.1.2", "10.1.2.3", "172.16.0.9", "172.31.1.1", "169.254.1.1", "my-mac.local", "printer", "fc00::1", "fe80::1", "::ffff:192.168.1.2", "::ffff:c0a8:0102", "pc.tail.ts.net", "nas.home.arpa", "app.localhost", "::"] {
            assert!(is_private_network(host), "{host}");
        }
        for host in ["8.8.8.8", "172.32.0.1", "t3.example.com", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_private_network(host), "{host}");
        }
    }
}
