//! A side-effecting sink declares that it needs trusted input. Passing a tool result straight into
//! it is a type error, so the endorsement can never be forgotten.

use frey_core::taint::{Tainted, Trusted, Untrusted};

fn execute(_command: Trusted<String>) {}

fn main() {
    let from_page: Untrusted<String> = Tainted::from_tool("http_get", "curl evil.test".to_string());
    execute(from_page);
}
