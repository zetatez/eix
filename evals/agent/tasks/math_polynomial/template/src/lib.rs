pub fn eval(coefficients: &[i64], x: i64) -> Option<i64> {
    let mut sum = 0;
    for (power, coefficient) in coefficients.iter().enumerate() {
        sum += coefficient * x.pow(power as u32);
    }
    Some(sum)
}

pub fn derivative(coefficients: &[i64]) -> Vec<i64> {
    coefficients[1..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_constant() {
        assert_eq!(eval(&[7], 5), Some(7));
    }
}
