use super::*;

#[test]
fn ssrf_predicate_blocks_internal_targets_and_allows_public() {
    let blocked = [
        "127.0.0.1",
        "::1",
        "169.254.169.254",
        "10.0.0.1",
        "192.168.1.1",
        "172.16.0.1",
        "fc00::1",
        "fe80::1",
        "0.0.0.0",
    ];
    for raw in blocked {
        let ip: IpAddr = raw.parse().expect("parse ip");
        assert!(ip_is_blocked(&ip), "expected {raw} to be blocked");
    }

    let allowed = [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "2606:4700:4700::1111",
    ];
    for raw in allowed {
        let ip: IpAddr = raw.parse().expect("parse ip");
        assert!(!ip_is_blocked(&ip), "expected {raw} to be allowed");
    }
}

#[tokio::test]
async fn ensure_url_allowed_rejects_loopback_and_metadata_and_localhost() {
    for raw in [
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5:8080/admin",
        "http://192.168.0.1/",
        "http://localhost:6379/",
    ] {
        let url = Url::parse(raw).expect("parse url");
        assert!(
            ensure_url_allowed(&url).await.is_err(),
            "expected {raw} to be refused"
        );
    }
}

#[tokio::test]
async fn ensure_url_allowed_permits_public_literal_ip() {
    let url = Url::parse("http://1.1.1.1/").expect("parse url");
    assert!(ensure_url_allowed(&url).await.is_ok());
}
