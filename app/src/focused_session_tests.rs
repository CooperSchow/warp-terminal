use super::*;

#[test]
fn writes_only_when_the_contents_change() {
    assert_eq!(file_contents(Some("abc")), "abc\n");
    assert_eq!(file_contents(None), "");

    let mut latest = None;
    // The first report always writes, even an empty one.
    assert!(replace_if_changed(&mut latest, file_contents(None)));
    assert!(!replace_if_changed(&mut latest, file_contents(None)));
    assert!(replace_if_changed(&mut latest, file_contents(Some("abc"))));
    assert!(!replace_if_changed(&mut latest, file_contents(Some("abc"))));
    assert!(replace_if_changed(&mut latest, file_contents(Some("def"))));
    assert!(replace_if_changed(&mut latest, file_contents(None)));
    assert_eq!(latest.as_deref(), Some(""));
}
