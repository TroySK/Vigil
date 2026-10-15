use vigil_core::views::fill;

#[test]
fn fill_replaces_all_placeholders_in_one_pass() {
    assert_eq!(
        fill(
            "[$STATUS] $NAME: $REASON ($TIME)",
            "A",
            false,
            "refused",
            "T"
        ),
        "[DOWN] A: refused (T)"
    );
    assert_eq!(fill("$NAME $NAME $STATUS", "x", true, "y", "z"), "x x UP");
    // Unknown tokens and a trailing $ pass through literally.
    assert_eq!(fill("$NOPE $? $", "x", true, "y", "z"), "$NOPE $? $");
    // Replacement text is literal: a value containing placeholder
    // syntax must not be re-expanded (chained replace() would turn
    // this into "UP").
    assert_eq!(fill("$NAME", "$STATUS", true, "", ""), "$STATUS");
}
