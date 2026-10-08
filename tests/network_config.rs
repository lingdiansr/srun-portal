use srun_portal::settings::parse_key_value;

#[test]
fn network_accepts_supported_modes() {
    assert_eq!(
        parse_key_value("network", "office")
            .expect("office")
            .to_string(),
        "\"office\""
    );
    assert_eq!(
        parse_key_value("network", "dorm")
            .expect("dorm")
            .to_string(),
        "\"dorm\""
    );
}

#[test]
fn network_rejects_unknown_modes() {
    assert!(parse_key_value("network", "auto").is_err());
    assert!(parse_key_value("network", "hotel").is_err());
}
