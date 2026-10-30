/// An exact fraction. The denominator is always positive and the fraction is
/// kept in lowest terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub fn new(num: i64, den: i64) -> Self {
        Self { num, den }
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            num: self.num * other.den + other.num * self.den,
            den: self.den * other.den,
        }
    }

    pub fn mul(self, other: Self) -> Self {
        Self {
            num: self.num * other.num,
            den: self.den * other.den,
        }
    }

    pub fn to_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_simple_fractions() {
        let sum = Rational::new(1, 2).add(Rational::new(1, 3));
        assert_eq!(sum.to_f64(), 5.0 / 6.0);
    }
}
