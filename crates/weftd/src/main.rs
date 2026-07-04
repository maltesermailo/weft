//! weftd — WEFT reference server. Placeholder until M1 (quinn acceptor,
//! session FSM, channel actors). M0 lives entirely in `weft-proto`.

fn main() {
    println!(
        "weftd {}: M0 codec only — server lands in M1",
        env!("CARGO_PKG_VERSION")
    );
}
