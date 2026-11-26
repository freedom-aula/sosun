//! Disassembles a dumped code piece: `VICTORY_PIECE=<file> [VICTORY_WAVE32=1] cargo run --example disasm`.

fn main() {
    let path = std::env::var("VICTORY_PIECE").expect("VICTORY_PIECE names a dumped code piece");
    let code = std::fs::read(path).expect("readable piece");
    let wave64 = std::env::var_os("VICTORY_WAVE32").is_none();
    match victory_shader::gfx10::decode(&code) {
        Ok(list) => {
            for i in list {
                println!("{:04x}: {}", i.offset, i.text(wave64));
            }
        }
        Err(e) => println!("decode failed: {e}"),
    }
}
