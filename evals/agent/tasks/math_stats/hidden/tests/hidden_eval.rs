use stats::{mean, median, variance};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn empty_is_none() {
    assert_eq!(mean(&[]), None);
    assert_eq!(variance(&[]), None);
    assert_eq!(median(&[]), None);
}

#[test]
fn non_finite_is_none() {
    assert_eq!(mean(&[1.0, f64::NAN]), None);
    assert_eq!(mean(&[1.0, f64::INFINITY]), None);
    assert_eq!(variance(&[1.0, f64::NAN]), None);
    assert_eq!(median(&[1.0, f64::INFINITY]), None);
}

#[test]
fn population_variance() {
    close(variance(&[1.0, 2.0, 3.0, 4.0]).unwrap(), 1.25);
    close(variance(&[5.0]).unwrap(), 0.0);
}

#[test]
fn median_averages_the_middle_pair() {
    close(median(&[1.0, 2.0, 3.0, 4.0]).unwrap(), 2.5);
    close(median(&[7.0]).unwrap(), 7.0);
    close(median(&[-1.0, 5.0, 3.0]).unwrap(), 3.0);
}
