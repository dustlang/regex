matset!(set1, &["a", "a"], "a", 0, 1);
matset!(set2, &["a", "a"], "ba", 0, 1);
matset!(set3, &["a", "b"], "a", 0);
matset!(set4, &["a", "b"], "b", 1);
matset!(set5, &["a|b", "b|a"], "b", 0, 1);
matset!(set6, &["foo", "oo"], "foo", 0, 1);
matset!(set7, &["^foo", "bar$"], "foo", 0);
matset!(set8, &["^foo", "bar$"], "foo bar", 0, 1);
matset!(set9, &["^foo", "bar$"], "bar", 1);
matset!(set10, &[r"[a-z]+$", "foo"], "01234 foo", 0, 1);
matset!(set11, &[r"[a-z]+$", "foo"], "foo 01234", 1);
matset!(set12, &[r".*?", "a"], "zzzzzza", 0, 1);
matset!(set13, &[r".*", "a"], "zzzzzza", 0, 1);
matset!(set14, &[r".*", "a"], "zzzzzz", 0);
matset!(set15, &[r"\ba\b"], "hello a bye", 0);
matset!(set16, &["a"], "a", 0);
matset!(set17, &[".*a"], "a", 0);
matset!(set18, &["a", "β"], "β", 1);

nomatset!(nset1, &["a", "a"], "b");
nomatset!(nset2, &["^foo", "bar$"], "bar foo");
nomatset!(nset3, { let xs: &[&str] = &[]; xs }, "a");

// See: https://github.com/rust-lang/regex/issues/187
#[test]
fn regression_subsequent_matches() {
    let set = regex_set!(&["ab", "b"]);
    let text = text!("ba");
    assert!(set.matches(text).matched(1));
    assert!(set.matches(text).matched(1));
}
