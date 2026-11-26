use vox_client::DevAddress;

#[test]
fn normalizes_dns_and_ipv6_and_retains_explicit_port() {
    let default = DevAddress::parse("VOX-LAB.test").unwrap();
    assert_eq!(default.authority(), "vox-lab.test:443");
    assert_eq!(
        default.profile_key(),
        DevAddress::parse("vox-lab.test:443").unwrap().profile_key()
    );
    assert_eq!(
        DevAddress::parse("[::1]:8443").unwrap().authority(),
        "[::1]:8443"
    );
    assert_ne!(
        default.profile_key(),
        DevAddress::parse("vox-lab.test:8443")
            .unwrap()
            .profile_key()
    );
}

#[test]
fn rejects_url_parts_and_non_bracketed_ipv6() {
    for input in [
        "",
        "https://vox.test",
        "vox.test/path",
        "vox.test#x",
        "user@vox.test",
        "::1",
        "[::1]:",
        "vox.test:0",
        "vox.test:+443",
        " vox.test",
        "vox.test..",
    ] {
        assert!(DevAddress::parse(input).is_err(), "{input}");
    }
}
