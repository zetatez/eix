pub fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n % 2 == 0 {
        return n == 2;
    }
    let mut d = 3;
    while d * d < n {
        if n % d == 0 {
            return false;
        }
        d += 2;
    }
    true
}

pub fn primes_up_to(n: u64) -> Vec<u64> {
    (2..=n).filter(|&x| is_prime(x)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small() {
        assert!(is_prime(7));
        assert_eq!(primes_up_to(10), vec![2, 3, 5, 7]);
    }
}
