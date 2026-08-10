//! Regenerates fixtures/conformance.json and prints the Move test vector
//! constants to paste into contracts/exchange/tests/conformance_tests.move.

fn main() {
    let vectors = exchange_signing::fixtures::generate();
    let json = serde_json::to_string_pretty(&vectors).unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/conformance.json");
    std::fs::write(path, &json).unwrap();
    println!("wrote {path}\n");

    for v in &vectors {
        println!("// vector: {}", v.name);
        println!("//   order_bcs      = x\"{}\"", v.order_bcs_hex);
        println!("//   digest         = x\"{}\"", v.digest_hex);
        println!("//   signing_digest = x\"{}\"", v.signing_digest_hex);
        println!("//   public_key     = x\"{}\"", v.public_key_hex);
        println!("//   signature      = x\"{}\"", v.signature_hex);
        println!("//   signer address = {}", v.signer_address);
        println!();
    }
}
