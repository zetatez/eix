pub fn factorial(n: u32) -> Option<String> {
    let mut value = 1u64;
    for factor in 2..=n {
        value *= factor as u64;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_factorials() {
        assert_eq!(factorial(5), Some("120".to_string()));
    }
}
