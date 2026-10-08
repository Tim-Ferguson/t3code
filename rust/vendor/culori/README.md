The literal parser, named color table, and canonical color-space conversion in
`crates/client/src/themes/color.rs` derive from Culori 4.0.2, the version used by
the original application. `LICENSE` preserves its MIT notice.

Source: https://github.com/Evercoder/culori/tree/v4.0.2/src

The Rust translation implements the original application's registered CSS color
profiles and parsing grammar. It preserves double-precision operations and the
application's separate gamut-mapping coefficients. The fixture generator executes
the pinned original library only to generate development parity fixtures; the
application does not load Culori or JavaScript color policy at runtime.
