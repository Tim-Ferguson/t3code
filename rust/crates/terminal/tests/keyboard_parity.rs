use serde::Deserialize;
use t3_terminal::keyboard::{KeyInput, key_for_code};
#[derive(Deserialize)]
struct Fixture {
    input: KeyInput,
    key: u32,
    consumed: u32,
    unshifted: u32,
}
#[test]
fn original_keyboard_modifier_and_unshifted_character_rules() {
    for (index, line) in include_str!("fixtures/keyboard.jsonl").lines().enumerate() {
        let case: Fixture = serde_json::from_str(line).unwrap();
        assert_eq!(
            (
                key_for_code(&case.input.code),
                case.input.consumed_mods(),
                case.input.unshifted()
            ),
            (case.key, case.consumed, case.unshifted),
            "keyboard witness {index}"
        );
    }
}
