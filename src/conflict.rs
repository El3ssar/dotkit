//! In-memory text conflict choices. Unchosen or malformed hunks can never be applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Choice {
    Local,
    Repo,
    Both,
}
#[derive(Clone, Debug)]
pub struct Hunk {
    pub local: String,
    pub repo: String,
    pub choice: Option<Choice>,
}
#[derive(Clone, Debug)]
enum Part {
    Text(String),
    Hunk(usize),
}
#[derive(Clone, Debug)]
pub struct Resolution {
    pub hunks: Vec<Hunk>,
    parts: Vec<Part>,
}
fn marker(line: &str, prefix: &str) -> bool {
    line.trim_end_matches(['\r', '\n'])
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}
impl Resolution {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut result = Self {
            hunks: vec![],
            parts: vec![],
        };
        let mut context = String::new();
        let mut local = String::new();
        let mut repo = String::new();
        let mut state = 0;
        for line in text.split_inclusive('\n') {
            if marker(line, "<<<<<<<") {
                if state != 0 {
                    return Err("Nested conflict markers require manual editing.".into());
                }
                result.parts.push(Part::Text(std::mem::take(&mut context)));
                state = 1;
            } else if marker(line, "|||||||") {
                if state != 1 {
                    return Err("Malformed conflict base marker.".into());
                }
                state = 2;
            } else if marker(line, "=======") {
                if !matches!(state, 1 | 2) {
                    return Err("Malformed conflict separator.".into());
                }
                state = 3;
            } else if marker(line, ">>>>>>>") {
                if state != 3 {
                    return Err("Malformed conflict end marker.".into());
                }
                result.parts.push(Part::Hunk(result.hunks.len()));
                result.hunks.push(Hunk {
                    local: std::mem::take(&mut local),
                    repo: std::mem::take(&mut repo),
                    choice: None,
                });
                state = 0;
            } else {
                match state {
                    0 => context.push_str(line),
                    1 => local.push_str(line),
                    3 => repo.push_str(line),
                    _ => {}
                }
            }
        }
        if state != 0 {
            return Err("Incomplete conflict markers; no resolution can be saved.".into());
        }
        result.parts.push(Part::Text(context));
        Ok(result)
    }
    pub fn remaining(&self) -> usize {
        self.hunks.iter().filter(|h| h.choice.is_none()).count()
    }
    pub fn output(&self) -> Result<String, String> {
        if self.remaining() != 0 {
            return Err(format!(
                "Choose a side for all {} remaining sections first.",
                self.remaining()
            ));
        }
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Hunk(i) => {
                    let h = &self.hunks[*i];
                    match h.choice.unwrap() {
                        Choice::Local => out.push_str(&h.local),
                        Choice::Repo => out.push_str(&h.repo),
                        Choice::Both => {
                            out.push_str(&h.local);
                            out.push_str(&h.repo);
                        }
                    }
                }
            }
        }
        if crate::core::has_conflict_markers(out.as_bytes()) {
            return Err("Result still contains conflict markers; edit this file manually.".into());
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn choices_preserve_context_line_endings_and_require_every_section() {
        let mut r=Resolution::parse("head\r\n<<<<<<< this machine\r\na\r\n||||||| base\r\nold\r\n=======\r\nb\r\n>>>>>>> repo\r\ntail").unwrap();
        assert!(r.output().is_err());
        r.hunks[0].choice = Some(Choice::Both);
        assert_eq!(r.output().unwrap(), "head\r\na\r\nb\r\ntail");
        r.hunks[0].choice = Some(Choice::Repo);
        assert_eq!(r.output().unwrap(), "head\r\nb\r\ntail");
    }
    #[test]
    fn rejects_nested_and_truncated_markers_and_keeps_empty_sides() {
        for text in [
            "<<<<<<< a\nx",
            "<<<<<<< a\n<<<<<<< nested\n=======\ny\n>>>>>>> z\n",
            "=======\n",
        ] {
            assert!(Resolution::parse(text).is_err());
        }
        let mut r = Resolution::parse("<<<<<<< a\n=======\nb\n>>>>>>> z\n").unwrap();
        r.hunks[0].choice = Some(Choice::Local);
        assert_eq!(r.output().unwrap(), "");
    }
}
