pub struct Config {
    pub retries: u32,
}

pub fn load() -> Config {
    Config { retries: 3 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_default() {
        assert_eq!(load().retries, 3);
    }
}
