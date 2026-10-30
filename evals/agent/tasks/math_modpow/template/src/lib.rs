pub fn mod_pow(base: u64, exp: u64, modulus: u64) -> u64 {
    base.pow(exp as u32) % modulus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_power() {
        assert_eq!(mod_pow(2, 10, 1000), 24);
    }
}
