//! `into_inner` exists only at high integrity and public confidentiality. Untrusted data cannot be
//! taken out and acted on, no matter how convenient that would be.

use frey_core::taint::{Tainted, Untrusted};

fn main() {
    let page: Untrusted<String> = Tainted::from_tool("http_get", "rm -rf /".to_string());
    let _raw: String = page.into_inner();
}
