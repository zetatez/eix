pub fn classify(a: f64, b: f64, c: f64) -> Option<&'static str> {
    if a == b && b == c {
        Some("equilateral")
    } else if a == b || b == c || a == c {
        Some("isosceles")
    } else {
        Some("scalene")
    }
}

pub fn area(a: f64, b: f64, c: f64) -> Option<f64> {
    Some(0.5 * a * b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_equilateral() {
        assert_eq!(classify(2.0, 2.0, 2.0), Some("equilateral"));
    }
}
