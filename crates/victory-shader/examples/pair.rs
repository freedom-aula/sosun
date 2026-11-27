//! The signatures of a guessed vertex and pixel shader pair as the host links them:
//! `VICTORY_VS=<container> VICTORY_PS=<container> cargo run --example pair`.

fn rows(dxil: &[u8], part: victory_shader::FourCC) -> Vec<String> {
    let c = victory_shader::Container::parse(dxil).expect("a container");
    let Some(p) = c.part(part) else { return vec![] };
    let d = p.data;
    let w = |at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
    (0..w(0) as usize)
        .map(|i| {
            let at = 8 + 32 * i;
            let name = d[w(at + 4) as usize..].split(|&b| b == 0).next().unwrap();
            format!(
                "{}{} sv{} reg{} mask{:x}",
                String::from_utf8_lossy(name),
                w(at + 8),
                w(at + 12),
                w(at + 20) as i32,
                d[at + 24]
            )
        })
        .collect()
}

fn main() {
    let read = |k: &str| std::fs::read(std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"))).expect("readable");
    let base = victory_shader::emit::Options { wave32_host: true, zero_copy_memory: true, ..Default::default() };
    let ps = victory_shader::emit::translate_with(&read("VICTORY_PS"), base).expect("pixel shader");
    println!("PS splits {:?}", ps.splits);
    println!("PS ISG1 {:?}", rows(&ps.dxil, victory_shader::FourCC::ISG1));
    println!("PS OSG1 {:?}", rows(&ps.dxil, victory_shader::FourCC::OSG1));
    let vs0 = victory_shader::emit::translate_with(&read("VICTORY_VS"), base).expect("vertex shader");
    println!("VS OSG1 (plain) {:?}", rows(&vs0.dxil, victory_shader::FourCC::OSG1));
    // As the host widens it: the pixel inputs the vertex shader lacks, written with the split's values.
    let mut copy = [0u8; 32];
    let mut extra = 0u32;
    for &(o, n) in &ps.splits {
        copy[n as usize] = o as u8 + 1;
        extra |= 1 << n;
    }
    let vs = victory_shader::emit::translate_with(&read("VICTORY_VS"), victory_shader::emit::Options { extra_params: extra, copy_params: copy, ..base }).expect("widened vertex shader");
    println!("VS OSG1 (widened) {:?}", rows(&vs.dxil, victory_shader::FourCC::OSG1));
}
