use complex::{abs, add, div, mul, Complex};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn multiplies_and_divides() {
    let product = mul(Complex::new(1.0, 2.0), Complex::new(3.0, -4.0));
    close(product.re, 11.0);
    close(product.im, 2.0);

    let quotient = div(Complex::new(1.0, 1.0), Complex::new(1.0, -1.0)).unwrap();
    close(quotient.re, 0.0);
    close(quotient.im, 1.0);
}

#[test]
fn handles_zero_and_magnitude() {
    assert_eq!(div(Complex::new(1.0, 1.0), Complex::new(0.0, 0.0)), None);
    close(abs(Complex::new(3.0, 4.0)), 5.0);
    let sum = add(Complex::new(1.0, 2.0), Complex::new(-1.0, 2.0));
    close(sum.re, 0.0);
    close(sum.im, 4.0);
}
