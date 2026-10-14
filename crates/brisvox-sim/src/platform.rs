//! Facts about the machine the tests run on, for the few expectations that are not the same on
//! every operating system and CPU (docs/TESTING.md, "Lessons"). Used by tests only.

/// Whether this machine runs the ONNX Runtime kernels that produced
/// `smart_turn_golden.json` (Python onnxruntime 1.28.0 on Windows x86_64 with VNNI integer
/// dot-product instructions), so the int8 Smart Turn model reproduces the goldens to their
/// rounding. Elsewhere it does not and no tolerance can say by how much: the model is chaotic in
/// the last bit (uint8 activation quantization turns a float rounding difference into a whole
/// quantization step). On Linux, same CPU and `ort` version, 2 of the 12 cases are 0.007 and
/// 0.096 off; without VNNI (emulated AVX2) up to 0.66 off with a flipped verdict
/// (docs/providers/smart-turn.md, "Platform dependence"). Tests compare to the goldens only
/// where this is `true`; everywhere they pin what does not depend on it (the features bit for
/// bit, two code paths on one machine bit for bit).
#[must_use]
pub fn smart_turn_goldens_apply_here() -> bool {
    cfg!(all(windows, target_arch = "x86_64")) && integer_dot_product_available()
}

#[cfg(target_arch = "x86_64")]
fn integer_dot_product_available() -> bool {
    std::arch::is_x86_feature_detected!("avx512vnni")
        || std::arch::is_x86_feature_detected!("avxvnni")
}

#[cfg(not(target_arch = "x86_64"))]
fn integer_dot_product_available() -> bool {
    false
}

/// What the kernel alone absorbs of data nobody reads on one loopback TCP connection, in bytes:
/// the sender's send buffer plus the receiver's receive buffer. A test that expects a server to
/// be held back by flow control ("the reader is bounded") cannot make this small from outside
/// the client, and it differs a lot by operating system: Windows loopback held about 0.5 MiB in
/// every measurement, Linux autotunes both ends up to the `tcp_rmem` / `tcp_wmem` maxima (6 MiB
/// and 4 MiB by default; 2.5 MiB in the receive queue and up to 8 MiB sent were observed before
/// the sender blocked). Other systems get 2 MiB.
#[must_use]
pub fn loopback_buffer_bytes() -> usize {
    #[cfg(target_os = "linux")]
    {
        let maximum = |file: &str| {
            std::fs::read_to_string(file)
                .ok()
                .and_then(|text| text.split_whitespace().nth(2)?.parse::<usize>().ok())
        };
        maximum("/proc/sys/net/ipv4/tcp_rmem").unwrap_or(6 << 20)
            + maximum("/proc/sys/net/ipv4/tcp_wmem").unwrap_or(4 << 20)
    }
    #[cfg(not(target_os = "linux"))]
    {
        2 << 20
    }
}
