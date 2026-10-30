use kinematics::{max_height, range, time_of_flight};

fn close(value: f64, expected: f64) {
    assert!(
        (value - expected).abs() < 1e-6,
        "got {value}, expected {expected}"
    );
}

#[test]
fn forty_five_degrees_is_the_documented_case() {
    close(range(10.0, 45.0, 9.81).unwrap(), 10.193_679_918_451);
    close(max_height(10.0, 45.0, 9.81).unwrap(), 2.548_419_979_613);
    close(time_of_flight(10.0, 45.0, 9.81).unwrap(), 1.441_604_039_116);
}

#[test]
fn ninety_degrees_has_no_range() {
    close(range(10.0, 90.0, 9.81).unwrap(), 0.0);
    close(max_height(10.0, 90.0, 9.81).unwrap(), 5.096_839_959);
}

#[test]
fn invalid_inputs_are_rejected() {
    assert_eq!(range(-1.0, 45.0, 9.81), Err("speed"));
    assert_eq!(range(f64::NAN, 45.0, 9.81), Err("speed"));
    assert_eq!(range(10.0, 45.0, 0.0), Err("gravity"));
    assert_eq!(range(10.0, 45.0, -9.81), Err("gravity"));
    assert_eq!(range(10.0, f64::INFINITY, 9.81), Err("angle"));
    assert_eq!(max_height(-1.0, 45.0, 9.81), Err("speed"));
    assert_eq!(time_of_flight(10.0, 45.0, 0.0), Err("gravity"));
}
