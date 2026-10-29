//! Minimal dylib plugin for the proposed ABI v1.
//!
//! Build with `cargo build -p eix-plugin-hello-dylib --release` and load
//! `target/release/libeix_plugin_hello_dylib.so` through the future
//! `DylibBackend`.

use eix_plugin_sdk::prelude::*;

#[derive(Default)]
struct HelloDylib;

impl Plugin for HelloDylib {
    fn describe(&self) -> Descriptor {
        Descriptor::new("hello-dylib")
            .version(env!("CARGO_PKG_VERSION"))
            .capabilities(["tools", "commands"])
            .tool(
                Tool::new(
                    "echo",
                    json!({
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                    }),
                )
                .description("Echo text back to the caller")
                .read_only(),
            )
            .command(Command::action("sync", "sync").description("Echo command arguments"))
    }

    fn call_tool(&mut self, ctx: &mut Ctx<'_>, name: &str, arguments: Value) -> Result<Value> {
        match name {
            "echo" => {
                let text = arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                ctx.log("info", &format!("echo: {text}"))?;
                Ok(json!({
                    "content": [{"type": "text", "text": text}],
                }))
            }
            other => Err(Error::UnknownMethod(other.to_string())),
        }
    }

    fn run_command(&mut self, ctx: &mut Ctx<'_>, command: &str, arguments: &str) -> Result<Value> {
        match command {
            "sync" => {
                ctx.log("info", &format!("sync: {arguments}"))?;
                Ok(json!({"ok": true, "output": format!("synced {arguments}")}))
            }
            other => Err(Error::UnknownMethod(other.to_string())),
        }
    }
}

eix_plugin!(HelloDylib);
