use wartui_proto::mac::{self, Mac};

const ADDRESS: Mac = [0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0];

#[test]
fn mac_formatter_writes_whole_and_short_forms_when_given_address() {
    let address = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
    assert_eq!(mac::full(&address).to_string(), "02:00:5E:10:57:84");
    assert_eq!(mac::short(&address).to_string(), "57:84");
}

#[test]
fn mac_formatter_joins_octets_when_given_three_octet_tail() {
    assert_eq!(mac::octets(&[0xEC, 0x44, 0xC0]).to_string(), "EC:44:C0");
}

#[test]
fn mac_formatter_prints_first_six_octets_when_given_longer_slice() {
    let bytes = [0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0, 0xFF];
    assert_eq!(mac::octets(&bytes).to_string(), "10:BD:A3:EC:44:C0");
}

#[test]
fn mac_formatter_pads_address_when_given_width() {
    // `wartui ports` lines addresses up in a column this way.
    assert_eq!(format!("{:<20}|", mac::full(&ADDRESS)), "10:BD:A3:EC:44:C0   |");
    assert_eq!(format!("{:>7}|", mac::short(&ADDRESS)), "  44:C0|");
}

#[test]
fn mac_parser_reads_address_when_serial_number_is_esp32_format() {
    assert_eq!(mac::parse("10:BD:A3:EC:44:C0"), Some(ADDRESS));
}

#[test]
fn mac_parser_accepts_text_when_lowercase() {
    assert_eq!(mac::parse("10:bd:a3:ec:44:c0"), Some(ADDRESS));
}

#[test]
fn mac_parser_round_trips_when_given_full_form() {
    assert_eq!(mac::parse(&mac::full(&ADDRESS).to_string()), Some(ADDRESS));
}

#[test]
fn mac_parser_rejects_text_when_serial_number_is_not_an_address() {
    // Everything on the bus that is not an ESP32 carries one of these.
    assert_eq!(mac::parse("0001"), None);
    assert_eq!(mac::parse(""), None);
}

#[test]
fn mac_parser_rejects_text_when_five_pairs() {
    assert_eq!(mac::parse("10:BD:A3:EC:44"), None);
}

#[test]
fn mac_parser_rejects_text_when_seven_pairs() {
    assert_eq!(mac::parse("10:BD:A3:EC:44:C0:FF"), None);
}

#[test]
fn mac_parser_rejects_text_when_a_pair_has_one_digit() {
    assert_eq!(mac::parse("1:BD:A3:EC:44:C0"), None);
}

#[test]
fn mac_parser_rejects_text_when_a_pair_has_a_sign() {
    // `from_str_radix` alone takes a sign, so this is a check of our own.
    assert_eq!(mac::parse("+0:BD:A3:EC:44:C0"), None);
    assert_eq!(mac::parse("-0:BD:A3:EC:44:C0"), None);
}

#[test]
fn mac_pair_reads_octet_when_exactly_two_hex_digits() {
    assert_eq!(mac::pair("c0"), Some(0xC0));
    assert_eq!(mac::pair("0F"), Some(0x0F));
    for text in ["", "8", "+8", "-8", "0g", "000"] {
        assert_eq!(mac::pair(text), None, "{text}");
    }
}

#[test]
fn mac_parser_rejects_text_when_not_hex() {
    assert_eq!(mac::parse("10:BD:A3:EC:44:CG"), None);
}
