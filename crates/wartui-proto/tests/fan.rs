//! The lid fan: its duty, its PWM wave and the tach reading.

use wartui_proto::fan::{FanDuty, Pwm, TACH_MIN_GAP_US, TachFilter, pwm, rpm};

/// A period of 40 µs at a 10 MHz tick.
const PERIOD: u16 = 400;

fn duty(pct: u8) -> FanDuty {
    FanDuty::new(pct).expect("in range")
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
