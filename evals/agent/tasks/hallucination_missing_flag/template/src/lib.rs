pub struct Config {
    pub verbose: bool,
    pub input: String,
}

pub fn parse_args(args: &[String]) -> Config {
    let mut config = Config {
        verbose: false,
        input: String::new(),
    };
    for arg in args {
        match arg.as_str() {
            "--verbose" => config.verbose = true,
            other => config.input = other.to_string(),
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbose() {
        let cfg = parse_args(&["--verbose".into(), "file.txt".into()]);
        assert!(cfg.verbose);
        assert_eq!(cfg.input, "file.txt");
    }
}
