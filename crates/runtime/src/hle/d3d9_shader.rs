//! Direct3D 9 shader bytecode (vs/ps 2.0 and 3.0) to GLSL ES 3.00.

/// Translates a shader's token stream (version token .. END) to GLSL.
pub fn translate(tokens: &[u32]) -> Result<String, String> {
    let _ = tokens;
    Err("shader translation is not implemented yet".into())
}
