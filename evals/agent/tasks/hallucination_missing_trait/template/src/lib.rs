pub struct Widget;

impl Widget {
    pub fn draw(&self) -> &'static str {
        "widget"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws() {
        assert_eq!(Widget.draw(), "widget");
    }
}
