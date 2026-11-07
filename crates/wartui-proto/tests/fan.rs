//! The lid fan: its duty, its PWM wave, the tach reading and the stored record.

use wartui_proto::fan::store::{self, MAGIC, Next, RECORDS, WORD};
use wartui_proto::fan::{FanDuty, Pwm, TACH_MIN_GAP_US, TachFilter, pwm, rpm};
use wartui_proto::ota::SECTOR;

/// A period of 40 µs at a 10 MHz tick.
const PERIOD: u16 = 400;

fn duty(pct: u8) -> FanDuty {
    FanDuty::new(pct).expect("in range")
}

/// `sector` with the record for `pct` written as the P4 writes it, at the next erased word.
fn write(sector: &mut [u8; SECTOR], pct: u8) {
    let Next::At(at) = store::next(sector) else { panic!("the sector is full") };
    let record = store::record(duty(pct));
    for (old, new) in sector[at..at + WORD].iter().zip(&record) {
        // NOR flash programs only by clearing bits: no write may set one.
        assert_eq!(new & !old, 0, "the record sets a bit at {at}");
    }
    sector[at..at + WORD].copy_from_slice(&record);
}

#[test]
fn pwm_holds_gpio_high_for_the_rest_of_the_period_when_duty_is_25() {
    // GPIO51 high holds the fan's PWM input low, so 25 % is GPIO51 high for 75 %.
    assert_eq!(pwm(duty(25), PERIOD), Pwm::Wave { gpio_high: 300, gpio_low: 100 });
}

#[test]
fn pwm_keeps_both_halves_when_duty_is_1_or_99() {
    for pct in [1, 99] {
        let Pwm::Wave { gpio_high, gpio_low } = pwm(duty(pct), PERIOD) else {
            panic!("{pct}% is a wave")
        };
        assert!(gpio_high >= 1 && gpio_low >= 1, "{pct}%: {gpio_high}/{gpio_low}");
        assert_eq!(gpio_high + gpio_low, PERIOD, "{pct}%");
    }
}

#[test]
fn pwm_holds_gpio_high_when_duty_is_0() {
    assert_eq!(pwm(duty(0), PERIOD), Pwm::GpioHigh);
}

#[test]
fn pwm_stops_when_duty_is_100() {
    assert_eq!(pwm(FanDuty::FULL, PERIOD), Pwm::Full);
}

#[test]
fn fan_duty_refuses_value_when_over_100() {
    assert_eq!(FanDuty::new(101), None);
    assert_eq!(FanDuty::new(100), Some(FanDuty::FULL));
    assert!(postcard::from_bytes::<FanDuty>(&[101]).is_err());
    assert_eq!(postcard::from_bytes::<FanDuty>(&[40]).expect("decodes"), duty(40));
}

#[test]
fn rpm_reads_fan_speed_when_window_counts_117_edges() {
    assert_eq!(rpm(117, 1000), 3510);
}

#[test]
fn rpm_reads_zero_when_window_counts_no_edge() {
    assert_eq!(rpm(0, 1000), 0);
    assert_eq!(rpm(5, 0), 0, "an empty window");
}

#[test]
fn rpm_saturates_when_count_is_past_u16() {
    assert_eq!(rpm(u32::MAX, 1000), u16::MAX);
}

#[test]
fn tach_filter_counts_once_when_edges_are_100_us_apart() {
    let mut filter = TachFilter::new();
    assert!(filter.accept(10_000));
    assert!(!filter.accept(10_100));
    assert!(!filter.accept(10_000 + TACH_MIN_GAP_US - 1));
    assert!(filter.accept(10_000 + TACH_MIN_GAP_US));
}

#[test]
fn fan_store_reads_no_duty_when_sector_is_erased() {
    let sector = [0xFF; SECTOR];
    assert_eq!(store::read(&sector), None);
    assert_eq!(store::next(&sector), Next::At(0));
}

#[test]
fn fan_store_reads_last_record_when_several_are_written() {
    let mut sector = [0xFF; SECTOR];
    for pct in [40, 75, 0] {
        write(&mut sector, pct);
    }
    assert_eq!(store::read(&sector), Some(duty(0)));
    assert_eq!(store::next(&sector), Next::At(3 * WORD));
}

#[test]
fn fan_store_skips_torn_word_when_it_sits_between_two_records() {
    let mut sector = [0xFF; SECTOR];
    write(&mut sector, 40);
    // A write a power cut stopped part-way: some bits cleared, the check not yet.
    sector[WORD..2 * WORD].copy_from_slice(&[MAGIC, 0x05, 0xFF, 0xFF]);
    write(&mut sector, 60);
    assert_eq!(store::read(&sector), Some(duty(60)));

    // The torn word last: the record before it stands.
    let mut sector = [0xFF; SECTOR];
    write(&mut sector, 40);
    sector[WORD..2 * WORD].copy_from_slice(&[MAGIC, 0x05, 0xFF, 0xFF]);
    assert_eq!(store::read(&sector), Some(duty(40)));
    assert_eq!(store::next(&sector), Next::At(2 * WORD));
}

#[test]
fn fan_store_ignores_record_when_duty_is_101_or_check_byte_is_bad() {
    let mut sector = [0xFF; SECTOR];
    write(&mut sector, 30);
    sector[WORD..2 * WORD].copy_from_slice(&[MAGIC, 101, !101, 0]);
    sector[2 * WORD..3 * WORD].copy_from_slice(&[MAGIC, 50, !51, 0]);
    assert_eq!(store::read(&sector), Some(duty(30)));
}

#[test]
fn fan_store_asks_for_erase_when_sector_is_full() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..RECORDS {
        write(&mut sector, u8::try_from(i % 101).expect("in range"));
    }
    assert_eq!(store::next(&sector), Next::Erase);
    assert_eq!(store::read(&sector), Some(duty(u8::try_from((RECORDS - 1) % 101).unwrap())));
}

#[test]
fn fan_store_writes_at_first_erased_word_when_one_is_left() {
    let mut sector = [0xFF; SECTOR];
    for _ in 0..RECORDS - 1 {
        write(&mut sector, 20);
    }
    assert_eq!(store::next(&sector), Next::At((RECORDS - 1) * WORD));
}
