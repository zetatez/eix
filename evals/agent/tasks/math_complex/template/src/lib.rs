#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }
}

pub fn add(a: Complex, b: Complex) -> Complex {
    Complex::new(a.re + b.re, a.im + b.im)
}

pub fn mul(a: Complex, b: Complex) -> Complex {
    Complex::new(a.re * b.re - a.im * b.im, a.re * b.im + a.im * b.re)
}

pub fn div(a: Complex, b: Complex) -> Option<Complex> {
    Some(Complex::new(a.re / b.re, a.im / b.im))
}

pub fn abs(a: Complex) -> f64 {
    (a.re * a.re + a.im * a.im).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_components() {
        assert_eq!(add(Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)), Complex::new(4.0, 6.0));
    }
}
